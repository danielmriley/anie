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
use futures::{FutureExt, StreamExt};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
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
use anie_providers_builtin::{
    ModelDiscoveryCache, ModelDiscoveryRequest, is_ollama_native_discovery_target,
    ollama_native_base_url,
};

// Measured before Set A Plan 09 PR A: interactive mode uses
// `mpsc::channel(256)`, so a saturated realistic streaming burst
// can already drain 256 events in one frame. Keep the cap at that
// observed burst size, counting the first awaited event.
pub(crate) const MAX_AGENT_EVENTS_PER_FRAME: usize = 256;

use crate::{
    InputPane, ModelPickerAction, ModelPickerPane, OnboardingAction, OnboardingCompletion,
    OnboardingScreen, OutputPane, ProviderManagementAction, ProviderManagementScreen,
    autocomplete::CommandCompletionProvider,
    commands::SlashCommandInfo,
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
    action_tx: mpsc::UnboundedSender<UiAction>,
    should_quit: bool,
    spinner: Spinner,
    last_ctrl_c: Option<Instant>,
    overlay: Option<Box<dyn OverlayScreen>>,
    known_models: Vec<Model>,
    clipboard: Option<arboard::Clipboard>,
    discovery_cache: Arc<Mutex<ModelDiscoveryCache>>,
    worker_tx: mpsc::UnboundedSender<AppWorkerEvent>,
    worker_rx: mpsc::UnboundedReceiver<AppWorkerEvent>,
    /// Catalog of slash-command metadata, owned upstream by the
    /// controller's `CommandRegistry`. Consulted by
    /// `handle_slash_command` for pre-dispatch validation and, in
    /// plan 12, by the inline autocomplete popup.
    commands: Vec<SlashCommandInfo>,
    /// Last time a `MessageDelta` arrived. Used by the
    /// stall-aware spinner logic in `needs_tick_redraw` — when
    /// streaming is stuck (no delta for > STREAM_STALL_WINDOW
    /// ms), idle-tick redraws are suppressed so the spinner
    /// freezes rather than eating CPU. Plan 06 PR-B.
    last_streaming_delta_at: Option<Instant>,
    /// Workspace-wide cap on Ollama `num_ctx` from
    /// `[ollama] default_max_num_ctx`. Applied to every
    /// `ModelInfo::to_model` call the TUI makes when
    /// converting a freshly-discovered `ModelInfo` into a
    /// runtime `Model` (model picker, post-discovery
    /// catalog updates). `None` (the default) preserves
    /// uncapped behavior. Set via
    /// `with_ollama_default_max_num_ctx` from
    /// `interactive_mode.rs` based on the loaded
    /// `AnieConfig`. See `docs/ollama_default_num_ctx_cap`.
    ollama_default_max_num_ctx: Option<u64>,
}

pub(crate) fn drain_agent_event_batch(
    event_rx: &mut mpsc::Receiver<AgentEvent>,
    first_event: AgentEvent,
) -> Vec<AgentEvent> {
    let mut events = Vec::with_capacity(MAX_AGENT_EVENTS_PER_FRAME);
    events.push(first_event);
    while events.len() < MAX_AGENT_EVENTS_PER_FRAME {
        let Ok(next) = event_rx.try_recv() else {
            break;
        };
        events.push(next);
    }
    events
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderMode {
    Full,
    UrgentInput,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RenderDirty {
    composer: bool,
    full: bool,
}

impl RenderDirty {
    const fn none() -> Self {
        Self {
            composer: false,
            full: false,
        }
    }

    const fn composer() -> Self {
        Self {
            composer: true,
            full: false,
        }
    }

    const fn full() -> Self {
        Self {
            composer: false,
            full: true,
        }
    }

    fn merge(&mut self, other: Self) {
        self.composer |= other.composer;
        self.full |= other.full;
    }

    const fn any(self) -> bool {
        self.composer || self.full
    }

    fn clear_after_render(&mut self, mode: RenderMode) {
        match mode {
            RenderMode::Full => {
                self.composer = false;
                self.full = false;
            }
            RenderMode::UrgentInput => {
                self.composer = false;
            }
        }
    }
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
    /// A context compaction is in flight — the LLM is
    /// summarizing the transcript. The wall-clock start is
    /// tracked so the status bar can show elapsed seconds.
    Compacting { started_at: std::time::Instant },
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
    SetResolvedModel(Box<Model>),
    /// Set the active thinking level.
    SetThinking(String),
    /// Query, set, or reset the Ollama native context-length override.
    ContextLength(Option<String>),
    /// Show a read-only summary of persistent values affecting the
    /// current model (effective context window, layered overrides,
    /// session id, persistent file paths).
    ShowState,
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
    /// Cached shortened cwd for the status bar. Recomputed
    /// only when `cwd` changes between paints — avoids the
    /// `env::var` + `Vec` allocation that used to fire on
    /// every render. Skipped during serialization-style
    /// equality checks since it's a derived view of `cwd`.
    cached_short_cwd: Option<(String, String)>,
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
            cached_short_cwd: None,
        }
    }
}

impl StatusBarState {
    /// Return the abbreviated cwd for the status bar, computing
    /// lazily and caching the (cwd, shortened) pair so repeat
    /// reads at the same cwd are O(1).
    fn shortened_cwd(&mut self) -> &str {
        let stale = self
            .cached_short_cwd
            .as_ref()
            .is_none_or(|(input, _)| input != &self.cwd);
        if stale {
            let computed = shorten_path(&self.cwd);
            self.cached_short_cwd = Some((self.cwd.clone(), computed));
        }
        self.cached_short_cwd
            .as_ref()
            .map_or("", |(_, shortened)| shortened.as_str())
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
    ///
    /// `commands` is the slash-command metadata catalog used for
    /// pre-dispatch validation (plan 11) and the inline
    /// autocomplete popup (plan 12). Pass
    /// `CommandRegistry::all().to_vec()` or equivalent.
    #[must_use]
    pub fn new(
        event_rx: mpsc::Receiver<AgentEvent>,
        action_tx: mpsc::UnboundedSender<UiAction>,
        initial_models: Vec<Model>,
        commands: Vec<SlashCommandInfo>,
    ) -> Self {
        let (worker_tx, worker_rx) = mpsc::unbounded_channel();
        // Attach the inline autocomplete provider so typing `/`
        // pops the command palette. Providers are wired here
        // rather than in `interactive_mode` because the TUI owns
        // the popup's lifecycle, and the set of commands is
        // already plumbed through `App::new` for pre-dispatch
        // validation.
        let input_pane = if commands.is_empty() {
            InputPane::new()
        } else {
            InputPane::new()
                .with_autocomplete(Arc::new(CommandCompletionProvider::new(commands.clone())))
        };
        Self {
            output_pane: OutputPane::new(),
            status_bar: StatusBarState::default(),
            input_pane,
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
            commands,
            last_streaming_delta_at: None,
            ollama_default_max_num_ctx: None,
        }
    }

    /// Snapshot the workspace-wide Ollama `num_ctx` cap from
    /// `[ollama] default_max_num_ctx`. The TUI applies it to
    /// every `to_model` conversion that builds a runtime
    /// `Model` from freshly-discovered `ModelInfo`. `None`
    /// preserves the prior uncapped behavior.
    #[must_use]
    pub fn with_ollama_default_max_num_ctx(mut self, cap: Option<u64>) -> Self {
        self.ollama_default_max_num_ctx = cap;
        self
    }

    /// Access the status bar state for setup and tests.
    pub fn status_bar_mut(&mut self) -> &mut StatusBarState {
        &mut self.status_bar
    }

    /// Enable or disable the inline slash-command autocomplete
    /// popup. When disabled, the input pane falls back to its
    /// pre-plan-12 behavior: history navigation on Up/Down, no
    /// popup on `/`. Pre-dispatch validation (plan 11) is
    /// unaffected — `/help` and command lookup still use the
    /// catalog.
    #[must_use]
    pub fn with_autocomplete_enabled(mut self, enabled: bool) -> Self {
        if !enabled {
            // Drop the provider by rebuilding the input pane from
            // scratch. `InputPane` is cheap to recreate; keeping
            // history would require copy-out/copy-in and at
            // startup there is no history yet.
            self.input_pane = InputPane::new();
        }
        self
    }

    /// Toggle markdown rendering for finalized assistant messages.
    /// Takes effect immediately — the output pane invalidates its
    /// line cache so the next frame re-renders with the new
    /// setting. Streaming blocks always render plain regardless;
    /// see the comment on `assistant_answer_lines` for rationale.
    #[must_use]
    pub fn with_markdown_enabled(mut self, enabled: bool) -> Self {
        self.output_pane.set_markdown_enabled(enabled);
        self
    }

    /// Set the interactive-transcript tool-output display mode
    /// at startup. See `OutputPane::set_tool_output_mode` for
    /// semantics.
    #[must_use]
    pub fn with_tool_output_mode(mut self, mode: anie_config::ToolOutputMode) -> Self {
        self.output_pane.set_tool_output_mode(mode);
        self
    }

    /// Record detected terminal capabilities so markdown
    /// rendering can tailor hyperlink / image emission to the
    /// terminal.
    #[must_use]
    pub fn with_terminal_capabilities(mut self, capabilities: crate::TerminalCapabilities) -> Self {
        self.output_pane.set_terminal_capabilities(capabilities);
        self
    }

    /// Flip markdown rendering at runtime. Used by the
    /// `/markdown on|off` slash command.
    pub fn set_markdown_enabled(&mut self, enabled: bool) {
        self.output_pane.set_markdown_enabled(enabled);
    }

    /// Runtime flip for the tool-output display mode. Used by
    /// the `/tool-output verbose|compact` slash command. Like
    /// `/markdown`, this is UI-only: no controller action
    /// dispatches.
    pub fn set_tool_output_mode(&mut self, mode: anie_config::ToolOutputMode) {
        self.output_pane.set_tool_output_mode(mode);
    }

    /// Current tool-output display mode.
    #[must_use]
    pub fn tool_output_mode(&self) -> anie_config::ToolOutputMode {
        self.output_pane.tool_output_mode()
    }

    /// Read-only view of the current agent UI state. Primarily
    /// for tests that verify transitions (`CompactionStart` →
    /// `Compacting`, etc.).
    #[must_use]
    pub fn agent_state(&self) -> &AgentUiState {
        &self.agent_state
    }

    /// Read-only view of the current transcript blocks in the
    /// output pane.
    #[must_use]
    pub fn output_blocks(&self) -> &[RenderedBlock] {
        self.output_pane.blocks()
    }

    /// Current input-pane contents. Intended for tests that
    /// assert on post-completion buffer state.
    #[must_use]
    pub fn input_pane_contents(&self) -> &str {
        self.input_pane.content()
    }

    /// Whether the inline autocomplete popup is currently open.
    #[must_use]
    pub fn input_pane_is_popup_open(&self) -> bool {
        self.input_pane.autocomplete_is_open()
    }

    /// Test-only helper. Production-path popup refreshes are
    /// debounced and fired from the render loop's
    /// `tick_autocomplete` call; unit tests that assert popup
    /// state without driving a render cycle call this to
    /// flush any pending refresh synchronously.
    #[cfg(test)]
    pub(crate) fn flush_pending_autocomplete_for_test(&mut self) {
        self.input_pane.flush_pending_autocomplete();
    }

    /// Preload a transcript without routing through streaming events.
    pub fn load_transcript(&mut self, messages: &[Message]) {
        for message in messages {
            self.load_message(message);
        }
    }

    /// Render the full app frame.
    pub fn render(&mut self, frame: &mut Frame<'_>) {
        self.render_with_mode(frame, RenderMode::Full);
    }

    fn render_with_mode(&mut self, frame: &mut Frame<'_>, mode: RenderMode) {
        // Fire any pending autocomplete refresh whose debounce
        // has elapsed. Cheap (one Option<Instant> comparison)
        // when nothing is pending.
        self.input_pane.tick_autocomplete();
        // `Spinner::tick` returns a `&'static str` — borrow it
        // directly instead of allocating a `String` per frame.
        // Plan 04 PR-F.
        let spinner_frame: &'static str = self.spinner.tick();
        let half_height = frame.area().height.saturating_sub(2).max(8) / 2;
        let bottom_height = match &self.bottom_pane {
            // +2 so the TOP/BOTTOM border rows the input pane
            // draws don't eat into the visible text area. The
            // `preferred_height` return value is the number of
            // *content* rows we want to show.
            BottomPane::Editor => {
                // Floor of 1 so the input starts a single row
                // tall and grows with content. PR 07 of
                // `docs/tui_polish_2026-04-26/` lowered the
                // floor in `InputPane::preferred_height`; this
                // caller's clamp had to come down to match —
                // otherwise the empty box still rendered
                // 3 rows tall.
                self.input_pane
                    .preferred_height(frame.area().width)
                    .clamp(1, 8)
                    + 2
            }
            BottomPane::ModelPicker(session) => session
                .picker
                .preferred_height(frame.area().width)
                .clamp(8, half_height.max(8)),
        };
        let (output_area, spinner_area, bottom_area, status_area) =
            layout(frame.area(), bottom_height);

        self.output_pane.render(
            output_area,
            frame.buffer_mut(),
            spinner_frame,
            matches!(mode, RenderMode::UrgentInput),
        );
        render_spinner_row(&self.agent_state, spinner_area, frame.buffer_mut());

        let input_locked = !matches!(self.agent_state, AgentUiState::Idle);
        let cursor = match &mut self.bottom_pane {
            BottomPane::Editor => {
                self.input_pane
                    .render(bottom_area, frame.buffer_mut(), input_locked)
            }
            BottomPane::ModelPicker(session) => {
                session
                    .picker
                    .render(bottom_area, frame.buffer_mut(), spinner_frame)
            }
        };
        render_status_bar(
            &mut self.status_bar,
            &self.agent_state,
            self.output_pane.is_scrolled(),
            status_area,
            frame.buffer_mut(),
        );
        frame.set_cursor_position(cursor);

        // Draw the inline autocomplete popup on top of the
        // existing layout. Only applies when the editor is active
        // — the model picker has its own UI.
        if matches!(self.bottom_pane, BottomPane::Editor)
            && let Some(popup) = self.input_pane.autocomplete_popup()
            && let Some(rect) = popup.layout_rect(frame.area(), bottom_area)
        {
            popup.render(rect, frame.buffer_mut());
        }

        if let Some(overlay) = &mut self.overlay {
            let area = frame.area();
            overlay.dispatch_render(frame, area);
        }
    }

    /// Render in `UrgentInput` mode. Equivalent to what the
    /// real event loop does for a keystroke paint, but exposed
    /// so unit tests and the criterion bench can exercise the
    /// same path without going through `run_tui`.
    pub fn render_urgent(&mut self, frame: &mut Frame<'_>) {
        self.render_with_mode(frame, RenderMode::UrgentInput);
    }

    /// Construct an `App` for benchmarking with a pre-populated
    /// `OutputPane`. The channel halves are dropped — callers
    /// must not enter `run_tui`; the intended use is timing
    /// `handle_terminal_event` + `render_urgent` against a
    /// `TestBackend`.
    #[doc(hidden)]
    #[must_use]
    pub fn for_bench(output_pane: OutputPane) -> Self {
        let (_event_tx, event_rx) = mpsc::channel(1);
        let (action_tx, _action_rx) = mpsc::unbounded_channel();
        let mut app = Self::new(event_rx, action_tx, Vec::new(), Vec::new());
        app.output_pane = output_pane;
        app
    }

    /// Handle an incoming terminal event.
    pub fn handle_terminal_event(&mut self, event: Event) -> Result<()> {
        self.handle_terminal_event_dirty(event).map(|_| ())
    }

    fn handle_terminal_event_dirty(&mut self, event: Event) -> Result<RenderDirty> {
        if self.overlay.is_some() {
            match event {
                Event::Key(key) => {
                    self.handle_overlay_key_event(key)?;
                    return Ok(RenderDirty::full());
                }
                Event::Resize(_, _) => return Ok(RenderDirty::full()),
                _ => {}
            }
            return Ok(RenderDirty::none());
        }

        match event {
            Event::Key(key) => {
                if matches!(self.bottom_pane, BottomPane::ModelPicker(_)) {
                    self.handle_model_picker_key(key);
                    Ok(RenderDirty::full())
                } else {
                    Ok(self.handle_key_event(key))
                }
            }
            Event::Mouse(mouse) => Ok(self.handle_mouse_event(mouse)),
            Event::Resize(_, _) => Ok(RenderDirty::full()),
            _ => Ok(RenderDirty::none()),
        }
    }

    /// Handle an incoming agent/controller event.
    /// Process a drained batch of agent events, coalescing
    /// consecutive `MessageDelta::TextDelta` /
    /// `MessageDelta::ThinkingDelta` runs into a single
    /// append per run. For fast streams this cuts the cache-
    /// invalidation rate from "once per delta" to "once per
    /// contiguous delta run" — saving `flat_cache_valid`
    /// bool flips and `invalidate_last` calls on the hot path.
    pub fn handle_agent_event_batch(&mut self, events: Vec<AgentEvent>) -> Result<()> {
        // Per-run accumulators; flushed when a non-delta
        // event arrives, when the delta kind changes, or at
        // the end of the batch.
        let mut pending_text = String::new();
        let mut pending_thinking = String::new();

        for event in events {
            match event {
                AgentEvent::MessageDelta {
                    delta: StreamDelta::TextDelta(text),
                } => {
                    if !pending_thinking.is_empty() {
                        self.output_pane
                            .append_thinking_to_last_assistant(&pending_thinking);
                        pending_thinking.clear();
                    }
                    pending_text.push_str(&text);
                    self.last_streaming_delta_at = Some(Instant::now());
                }
                AgentEvent::MessageDelta {
                    delta: StreamDelta::ThinkingDelta(text),
                } => {
                    if !pending_text.is_empty() {
                        self.output_pane.append_to_last_assistant(&pending_text);
                        pending_text.clear();
                    }
                    pending_thinking.push_str(&text);
                    self.last_streaming_delta_at = Some(Instant::now());
                }
                // Any other event flushes both accumulators
                // first so message-level ordering is preserved.
                other => {
                    if !pending_text.is_empty() {
                        self.output_pane.append_to_last_assistant(&pending_text);
                        pending_text.clear();
                    }
                    if !pending_thinking.is_empty() {
                        self.output_pane
                            .append_thinking_to_last_assistant(&pending_thinking);
                        pending_thinking.clear();
                    }
                    self.handle_agent_event(other)?;
                }
            }
        }
        if !pending_text.is_empty() {
            self.output_pane.append_to_last_assistant(&pending_text);
        }
        if !pending_thinking.is_empty() {
            self.output_pane
                .append_thinking_to_last_assistant(&pending_thinking);
        }
        Ok(())
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) -> Result<()> {
        match event {
            AgentEvent::AgentStart => {
                self.agent_state = AgentUiState::Streaming;
                // Reset the stall tracker so a previous
                // stream's last delta doesn't make the new
                // stream appear stalled from the start.
                self.last_streaming_delta_at = None;
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
            AgentEvent::MessageDelta { delta } => {
                match delta {
                    StreamDelta::TextDelta(text) => {
                        self.output_pane.append_to_last_assistant(&text)
                    }
                    StreamDelta::ThinkingDelta(text) => {
                        self.output_pane.append_thinking_to_last_assistant(&text)
                    }
                    _ => {}
                }
                self.last_streaming_delta_at = Some(Instant::now());
            }
            AgentEvent::MessageEnd { message } => {
                if let Message::Assistant(assistant) = message {
                    self.output_pane.finalize_last_assistant(
                        extract_text(&assistant.content),
                        extract_thinking(&assistant.content),
                        assistant.timestamp,
                        assistant.error_message.clone(),
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
                // Permanent record in the transcript + transition
                // the agent state so the status bar shows the
                // live elapsed counter while the summarization
                // LLM call is in flight.
                self.output_pane
                    .add_system_message("Compacting context…".to_string());
                self.agent_state = AgentUiState::Compacting {
                    started_at: std::time::Instant::now(),
                };
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
                // Only return to Idle when no streaming / tool run
                // is queued behind the compaction (the controller
                // doesn't do this today but the guard costs
                // nothing and keeps state transitions predictable).
                if matches!(self.agent_state, AgentUiState::Compacting { .. }) {
                    self.agent_state = AgentUiState::Idle;
                }
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

    /// Whether the next periodic tick should trigger a redraw.
    ///
    /// The tick is the heartbeat that advances the spinner and
    /// polls overlay workers. When there is no active run, no
    /// overlay, and no pending worker output, the tick changes
    /// nothing visible — redrawing anyway would re-wrap the
    /// entire transcript 10 times a second for no reason. We
    /// return `true` only when one of those signals is live.
    ///
    /// Stall-aware spinner (Plan 06 PR-B): if we're in a
    /// streaming state but no delta has arrived for
    /// STREAM_STALL_WINDOW, suppress the redraw so a stuck
    /// stream doesn't eat CPU animating a spinner. The spinner
    /// appears frozen — which is accurate; the stream *is*
    /// stalled. Any arriving delta immediately updates
    /// `last_streaming_delta_at` and the spinner resumes.
    #[must_use]
    pub fn needs_tick_redraw(&self) -> bool {
        if self.overlay.is_some() {
            return true;
        }
        if matches!(self.agent_state, AgentUiState::Idle) {
            return false;
        }
        if matches!(self.agent_state, AgentUiState::Streaming)
            && self
                .last_streaming_delta_at
                .is_some_and(|t| t.elapsed() >= STREAM_STALL_WINDOW)
        {
            return false;
        }
        true
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> RenderDirty {
        // Keys that work the same regardless of agent state:
        // PageUp/PageDown scroll, Ctrl+O opens the model
        // picker. Try those first so the per-state handlers
        // don't have to repeat the wiring.
        if let Some(dirty) = self.try_shared_scroll_or_picker(key) {
            return dirty;
        }
        match self.agent_state {
            AgentUiState::Idle => self.handle_idle_key(key),
            AgentUiState::Streaming
            | AgentUiState::ToolExecuting { .. }
            | AgentUiState::Compacting { .. } => self.handle_active_key(key),
        }
    }

    /// Handle keys whose behavior is identical across all agent
    /// states. Returns `Some` if the key was consumed.
    fn try_shared_scroll_or_picker(&mut self, key: KeyEvent) -> Option<RenderDirty> {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                self.output_pane.scroll_page_up();
                Some(RenderDirty::full())
            }
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                self.output_pane.scroll_page_down();
                Some(RenderDirty::full())
            }
            (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
                self.open_model_picker_for_current_provider(None);
                Some(RenderDirty::full())
            }
            _ => None,
        }
    }

    fn handle_idle_key(&mut self, key: KeyEvent) -> RenderDirty {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c'))
            | (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                self.should_quit = true;
                let _ = self.action_tx.send(UiAction::Quit);
                RenderDirty::none()
            }
            // Home/End scroll only when the editor is empty.
            // Otherwise they belong to the input pane (jump to
            // start/end of the current line).
            (KeyModifiers::NONE, KeyCode::Home) if self.input_pane.content().is_empty() => {
                self.output_pane.scroll_to_top();
                RenderDirty::full()
            }
            (KeyModifiers::NONE, KeyCode::End) if self.input_pane.content().is_empty() => {
                self.output_pane.scroll_to_bottom();
                RenderDirty::full()
            }
            (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                self.output_pane.clear();
                let _ = self.action_tx.send(UiAction::ClearOutput);
                RenderDirty::full()
            }
            _ => self.handle_editor_key(key),
        }
    }

    fn handle_active_key(&mut self, key: KeyEvent) -> RenderDirty {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                if self
                    .last_ctrl_c
                    .is_some_and(|instant| instant.elapsed() <= Duration::from_secs(2))
                {
                    self.should_quit = true;
                    let _ = self.action_tx.send(UiAction::Quit);
                    RenderDirty::none()
                } else {
                    self.last_ctrl_c = Some(Instant::now());
                    self.output_pane
                        .add_system_message("Aborting... (press Ctrl+C again to quit)".to_string());
                    let _ = self.action_tx.send(UiAction::Abort);
                    RenderDirty::full()
                }
            }
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                self.should_quit = true;
                let _ = self.action_tx.send(UiAction::Quit);
                RenderDirty::none()
            }
            // Active state: Home/End scroll unconditionally —
            // the editor is locked while the agent is running,
            // so input-line navigation is irrelevant.
            (KeyModifiers::NONE, KeyCode::Home) => {
                self.output_pane.scroll_to_top();
                RenderDirty::full()
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                self.output_pane.scroll_to_bottom();
                RenderDirty::full()
            }
            _ => RenderDirty::none(),
        }
    }

    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> RenderDirty {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.output_pane.scroll_line_up(3);
                RenderDirty::full()
            }
            MouseEventKind::ScrollDown => {
                self.output_pane.scroll_line_down(3);
                RenderDirty::full()
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                self.handle_left_click(mouse.row, mouse.column);
                RenderDirty::none()
            }
            _ => RenderDirty::none(),
        }
    }

    /// Handle a left-click in the output pane — currently only
    /// used for clickable URL hit-testing. Clicks on prose are
    /// inert; users expect text regions to do nothing in a TUI
    /// with mouse capture.
    fn handle_left_click(&mut self, row: u16, col: u16) {
        if let Some(url) = self
            .output_pane
            .url_at_terminal_position(row, col)
            .map(str::to_string)
            && let Err(err) = opener::open_browser(&url)
        {
            // Don't spam the transcript with failures; a
            // debug log is enough. Users will notice if their
            // browser doesn't open.
            tracing::debug!(%err, %url, "failed to open clicked URL");
        }
    }

    fn handle_submit(&mut self, text: String) -> RenderDirty {
        let trimmed = text.trim();
        if trimmed.starts_with('/') {
            self.handle_slash_command(trimmed);
            RenderDirty::full()
        } else {
            let _ = self.action_tx.send(UiAction::SubmitPrompt(text));
            RenderDirty::composer()
        }
    }

    fn handle_editor_key(&mut self, key: KeyEvent) -> RenderDirty {
        match self.input_pane.handle_key(key) {
            InputAction::Submit(text) => self.handle_submit(text),
            InputAction::None => RenderDirty::composer(),
        }
    }

    #[cfg(test)]
    pub(crate) fn output_flat_build_count(&self) -> u64 {
        self.output_pane.flat_build_count()
    }

    fn handle_slash_command(&mut self, command: &str) {
        let mut parts = command.splitn(2, char::is_whitespace);
        let raw_cmd = parts.next().unwrap_or_default();
        let arg = parts
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let name = raw_cmd.strip_prefix('/').unwrap_or(raw_cmd);

        // `/exit` is an alias for `/quit` that predates the
        // catalog; keep it working without adding a duplicate
        // entry (duplicates would confuse autocomplete in plan 12).
        if name == "exit" {
            self.should_quit = true;
            let _ = self.action_tx.send(UiAction::Quit);
            return;
        }

        let Some(info) = self.commands.iter().find(|info| info.name == name).cloned() else {
            self.output_pane.add_system_message(format!(
                "Unknown command: {raw_cmd}. Type /help for available commands."
            ));
            return;
        };

        if let Err(message) = info.validate(arg) {
            self.output_pane.add_system_message(message);
            return;
        }

        self.dispatch_validated_command(&info, arg);
    }

    /// Dispatch a well-formed slash command.
    ///
    /// Preconditions enforced by `handle_slash_command`:
    /// - `info.name == raw_cmd.strip_prefix('/').unwrap_or(raw_cmd)`
    /// - `info.validate(arg)` returned `Ok(())`.
    ///
    /// If a new builtin is added to `builtin_commands()`, wire it
    /// here too and update the coverage test in `anie-cli/src/
    /// commands.rs` (`registry_covers_every_dispatched_slash_command`).
    /// No-arg commands that simply forward a `UiAction` go in
    /// `fixed_noarg_action` below; everything else (commands
    /// with custom dispatch, side effects, or argument parsing)
    /// stays in the main match.
    fn dispatch_validated_command(&mut self, info: &SlashCommandInfo, arg: Option<&str>) {
        if let Some(action) = fixed_noarg_action(info.name) {
            let _ = self.action_tx.send(action);
            return;
        }
        match info.name {
            "model" => match arg {
                None => self.open_model_picker_for_current_provider(None),
                Some(query) => {
                    if !self.try_exact_model_switch(query) {
                        self.open_model_picker_for_current_provider(Some(query.to_string()));
                    }
                }
            },
            "thinking" => match arg {
                None => self.output_pane.add_system_message(format!(
                    "Current thinking level: {}",
                    self.status_bar.thinking,
                )),
                Some(level) => {
                    let _ = self
                        .action_tx
                        .send(UiAction::SetThinking(level.to_string()));
                }
            },
            "context-length" => {
                let _ = self
                    .action_tx
                    .send(UiAction::ContextLength(arg.map(str::to_string)));
            }
            "clear" => {
                self.output_pane.clear();
                let _ = self.action_tx.send(UiAction::ClearOutput);
            }
            "session" => match arg {
                None => {
                    let _ = self.action_tx.send(UiAction::GetState);
                }
                Some("list") => {
                    let _ = self.action_tx.send(UiAction::ListSessions);
                }
                Some(session_id) => {
                    let _ = self
                        .action_tx
                        .send(UiAction::SwitchSession(session_id.to_string()));
                }
            },
            "onboard" => self.open_onboarding_overlay(),
            "providers" => self.open_provider_management_overlay(),
            "copy" => self.copy_last_assistant_to_clipboard(),
            "markdown" => match arg {
                None => {
                    let state = if self.output_pane.markdown_enabled() {
                        "on"
                    } else {
                        "off"
                    };
                    self.output_pane
                        .add_system_message(format!("Markdown rendering is {state}."));
                }
                Some("on") => {
                    self.output_pane.set_markdown_enabled(true);
                    self.output_pane
                        .add_system_message("Markdown rendering enabled.".to_string());
                }
                Some("off") => {
                    self.output_pane.set_markdown_enabled(false);
                    self.output_pane
                        .add_system_message("Markdown rendering disabled.".to_string());
                }
                Some(other) => {
                    self.output_pane.add_system_message(format!(
                        "Unknown /markdown argument: {other}. Expected on|off."
                    ));
                }
            },
            "tool-output" => match arg {
                None => {
                    // Report current state, same shape as /markdown.
                    let state = match self.output_pane.tool_output_mode() {
                        anie_config::ToolOutputMode::Verbose => "verbose",
                        anie_config::ToolOutputMode::Compact => "compact",
                    };
                    self.output_pane
                        .add_system_message(format!("Tool output mode is {state}."));
                }
                Some("verbose") => {
                    self.output_pane
                        .set_tool_output_mode(anie_config::ToolOutputMode::Verbose);
                    self.output_pane.add_system_message(
                        "Tool output mode set to verbose (full bodies).".to_string(),
                    );
                }
                Some("compact") => {
                    self.output_pane
                        .set_tool_output_mode(anie_config::ToolOutputMode::Compact);
                    self.output_pane.add_system_message(
                        "Tool output mode set to compact (titles only for bash/read).".to_string(),
                    );
                }
                Some(other) => {
                    self.output_pane.add_system_message(format!(
                        "Unknown /tool-output argument: {other}. Expected verbose|compact."
                    ));
                }
            },
            "login" => match arg {
                Some(provider) => {
                    // OAuth login needs a browser callback server
                    // on a specific port per provider. That would
                    // block the TUI event loop and doesn't play
                    // nicely with the alternate-screen terminal
                    // state, so we redirect the user to the CLI
                    // flow rather than running it in-process.
                    self.output_pane.add_system_message(format!(
                        "To log in to {provider}, exit anie (Ctrl-C) and run \
                         `anie login {provider}` in a regular shell. \
                         When the flow completes, your credential will be \
                         picked up automatically on the next `anie` run."
                    ));
                }
                None => {
                    self.output_pane.add_system_message(
                        "Usage: /login <provider>. Providers that support \
                         OAuth login: anthropic, openai-codex, github-copilot, \
                         google-antigravity, google-gemini-cli."
                            .to_string(),
                    );
                }
            },
            "logout" => match arg {
                Some(provider) => {
                    // Logout is synchronous and safe to run
                    // in-process — no browser, no callback server.
                    let store = anie_auth::CredentialStore::new();
                    match store.get_credential(provider) {
                        None => {
                            self.output_pane.add_system_message(format!(
                                "No stored credential for {provider}."
                            ));
                        }
                        Some(_) => match store.delete(provider) {
                            Ok(()) => self.output_pane.add_system_message(format!(
                                "Removed stored credential for {provider}."
                            )),
                            Err(error) => self.output_pane.add_system_message(format!(
                                "Failed to remove credential for {provider}: {error}"
                            )),
                        },
                    }
                }
                None => {
                    self.output_pane
                        .add_system_message("Usage: /logout <provider>".to_string());
                }
            },
            "quit" => {
                self.should_quit = true;
                let _ = self.action_tx.send(UiAction::Quit);
            }
            other => {
                // The catalog is authoritative for name lookup, so
                // reaching this arm means a command was registered
                // without a matching dispatch case. Surface that
                // loudly rather than swallow silently.
                self.output_pane.add_system_message(format!(
                    "Command /{other} has no handler. This is a bug; please report it."
                ));
            }
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
                    .send(UiAction::SetResolvedModel(Box::new(model.clone())));
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
                                let api = discovery_model_api(&provider_name, api, &base_url);
                                self.known_models.push(model.to_model(
                                    api,
                                    &discovery_model_base_url(api, &base_url),
                                    self.ollama_default_max_num_ctx,
                                ));
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
            .send(UiAction::SetResolvedModel(Box::new(model.clone())));
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
            .unwrap_or_else(|| {
                let api =
                    discovery_model_api(&context.provider_name, context.api, &context.base_url);
                model_info.to_model(
                    api,
                    &discovery_model_base_url(api, &context.base_url),
                    self.ollama_default_max_num_ctx,
                )
            })
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
        // Provider-specific discovery headers (e.g. Copilot's
        // editor identifiers) ride along on the same registry
        // we use for chat requests.
        let request = ModelDiscoveryRequest {
            provider_name: context.provider_name.clone(),
            api: context.api,
            base_url: context.base_url.clone(),
            api_key,
            headers: anie_auth::oauth_request_headers(&context.provider_name),
        };
        let cache = Arc::clone(&self.discovery_cache);
        let tx = self.worker_tx.clone();
        tokio::spawn(async move {
            // Plan 06 PR-E: cache now returns Arc<[ModelInfo]>.
            // Convert to Vec at the event boundary so the
            // AppWorkerEvent payload stays owned data; the
            // Arc-sharing benefit applies to in-cache lookups,
            // not to one-shot worker events.
            let result = cache
                .lock()
                .await
                .refresh(&request)
                .await
                .map(|models| models.to_vec())
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
                                .send(UiAction::ReloadConfig { provider, model });
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
                        let _ = self.action_tx.send(UiAction::ReloadConfig {
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
                    .send(UiAction::ReloadConfig { provider, model });
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
                    error_message: assistant.error_message.clone(),
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
/// Cap redraws at ~120 fps. Per-keystroke paint cost measured
/// at ~420 µs in `tui_render::keystroke_during_stream_600`, so
/// the cap exists to bound CPU during streaming bursts, not to
/// throttle individual keystrokes. An 8 ms cap puts worst-case
/// keystroke→paint latency at ~7.6 ms (avg ~4 ms), comfortably
/// below the ~15 ms perceptual threshold even for fast typists,
/// while still capping render rate during a burst of upstream
/// agent events. Earlier values (33 ms, then 16 ms) left
/// detectable input lag.
const FRAME_BUDGET: Duration = Duration::from_millis(8);

/// Idle poll interval when nothing is dirty. Matches the previous
/// behavior so background worker polling cadence is unchanged.
const IDLE_TICK: Duration = Duration::from_millis(100);

/// How long of a streaming-delta gap makes us declare the
/// stream "stalled" and suppress spinner-only redraws. 500 ms
/// is below typical timeout perception — a real stall holds
/// much longer than this — but far enough above the normal
/// inter-token interval (~10-100 ms) that healthy streams
/// never trip it. Plan 06 PR-B.
const STREAM_STALL_WINDOW: Duration = Duration::from_millis(500);

/// Debounce window between terminal `Resize` events and the
/// next full re-render. Drags (window-edge drag, tmux pane
/// resize, terminal maximize) fire bursts of `Resize` events
/// at ~100/s; each one invalidates every block's cache at the
/// new width, so rendering one per intermediate size spends
/// ~100 ms/frame rebuilding markdown for no visible benefit.
/// Skipping paints inside a 50 ms window after the most recent
/// resize means drag-in-progress stays cheap and the final
/// size gets a single rebuild. Plan 05 PR-C.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(50);

pub async fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut term_events = EventStream::new();
    // Request-based rendering: handlers set `dirty` when they
    // change visible state; the render below draws as soon as
    // the constraints allow. See
    // docs/tui_responsiveness/01_render_scheduling.md.
    let mut dirty = RenderDirty {
        composer: false,
        full: true,
    };
    // Typed input needs sub-frame-budget response or the user
    // feels lag. Set when a terminal event (keystroke, paste,
    // focus change) arrived — we bypass FRAME_BUDGET for
    // exactly one render cycle so every keystroke paints
    // immediately. Streaming / tick paths still respect the
    // budget for coalescing.
    let mut input_urgent = false;
    let mut last_render_at = Instant::now()
        .checked_sub(FRAME_BUDGET)
        .unwrap_or_else(Instant::now);
    // When set, the most recent terminal `Resize`. The render
    // gate waits `RESIZE_DEBOUNCE` from this instant before
    // repainting; cleared on the next successful paint.
    let mut last_resize_at: Option<Instant> = None;
    // Opt-in tracing of keystroke → paint latency. Set
    // `ANIE_TRACE_TYPING=1` and a line is emitted to the log
    // file every keystroke-driven paint with the measured
    // `t_key_to_paint_us`. Off by default (one atomic load
    // per loop when unset).
    let trace_typing = std::env::var("ANIE_TRACE_TYPING")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    // Time of the most recent key arrival in the term-events
    // branch. `None` once its paint completes.
    let mut key_arrival_at: Option<Instant> = None;

    loop {
        // Two gates: frame budget (coalesce streaming bursts)
        // AND resize debounce (skip mid-drag paints). Both
        // gated on `!input_urgent` so typing skips them.
        let resize_ready = match last_resize_at {
            Some(t) => t.elapsed() >= RESIZE_DEBOUNCE,
            None => true,
        };
        let budget_ready = input_urgent || last_render_at.elapsed() >= FRAME_BUDGET;
        if dirty.any() && budget_ready && resize_ready {
            let frame = crate::render_debug::RenderFrame::begin();
            let render_mode = if input_urgent {
                RenderMode::UrgentInput
            } else {
                RenderMode::Full
            };
            // Fast-path keystroke paints bypass the DECSET 2026
            // synchronized-output wrap: a single typed char
            // changes only a handful of cells and the tearing
            // sync prevents isn't visible on that scale, while
            // the wrap itself can add a VSync-alignment wait on
            // GPU terminals. Non-urgent paints (streaming,
            // scrolling, resize-final) still wrap for atomic
            // composition.
            if matches!(render_mode, RenderMode::UrgentInput) {
                crate::terminal::draw_urgent(terminal, |f| app.render_with_mode(f, render_mode))?;
            } else {
                crate::terminal::draw_synchronized(terminal, |f| {
                    app.render_with_mode(f, render_mode)
                })?;
            }
            frame.end(app.output_pane.blocks().len());
            if trace_typing && let Some(arrived) = key_arrival_at.take() {
                let us = arrived.elapsed().as_micros();
                tracing::info!(
                    target: "anie_tui::input_latency",
                    t_key_to_paint_us = us as u64,
                    urgent = input_urgent,
                    "keystroke paint",
                );
            }
            dirty.clear_after_render(render_mode);
            input_urgent = false;
            last_render_at = Instant::now();
            last_resize_at = None;
        }

        // Wait at most until the next frame opportunity (when
        // dirty) or until the next idle tick (when clean).
        // When resize-debouncing, wait at least until the
        // debounce elapses so the select loop doesn't spin.
        let timeout = if dirty.any() {
            let budget_remaining = FRAME_BUDGET.saturating_sub(last_render_at.elapsed());
            if let Some(t) = last_resize_at {
                let debounce_remaining = RESIZE_DEBOUNCE.saturating_sub(t.elapsed());
                budget_remaining.max(debounce_remaining)
            } else {
                budget_remaining
            }
        } else {
            IDLE_TICK
        };

        tokio::select! {
            Some(Ok(event)) = term_events.next() => {
                let mut saw_resize = matches!(event, Event::Resize(_, _));
                let mut render_dirty = app.handle_terminal_event_dirty(event)?;
                // Drain any terminal events already buffered in
                // the stream. Mouse-motion tracking fires at
                // ~100 events/sec while the cursor moves; without
                // this drain we'd spin the select loop once per
                // event, starving higher-signal events (keystrokes,
                // agent deltas) even though we filter the mouse
                // ones out of the dirty flag.
                while let Some(Some(next)) = term_events.next().now_or_never() {
                    let event = next?;
                    if matches!(event, Event::Resize(_, _)) {
                        saw_resize = true;
                    }
                    render_dirty.merge(app.handle_terminal_event_dirty(event)?);
                }
                if saw_resize {
                    last_resize_at = Some(Instant::now());
                }
                if render_dirty.composer {
                    // Typed input needs immediate paint; bypass
                    // FRAME_BUDGET for exactly one frame so the
                    // keystroke lands on screen without waiting
                    // out the 33 ms coalescing budget.
                    input_urgent = true;
                    if trace_typing && key_arrival_at.is_none() {
                        key_arrival_at = Some(Instant::now());
                    }
                }
                dirty.merge(render_dirty);
            }
            Some(event) = app.event_rx.recv() => {
                // Drain any additional agent events that piled up
                // while we were busy — without this, a burst of
                // N TextDelta events during fast streaming forces
                // N full redraws. One coalesced redraw per burst
                // keeps keystroke latency bounded even when the
                // agent is emitting tokens at hundreds/sec.
                //
                // Phase 3.1 (tui_perf 04 PR-A): consecutive text
                // and thinking deltas inside the drained batch
                // collapse into a single `append_to_last_assistant`
                // call each, so the block cache invalidates once
                // per contiguous delta-run rather than once per
                // delta.
                let events = drain_agent_event_batch(&mut app.event_rx, event);
                app.handle_agent_event_batch(events)?;
                dirty.full = true;
            }
            _ = tokio::time::sleep(timeout) => {
                app.handle_tick()?;
                // Only mark dirty when the tick actually changed
                // something visible — during idle there is no
                // spinner and no worker output, so suppressing
                // these redraws eliminates 10fps background work
                // that otherwise scales with transcript size.
                if app.needs_tick_redraw() {
                    dirty.full = true;
                }
            }
        }

        if app.should_quit() {
            break;
        }
    }
    Ok(())
}

/// Vertical layout: output transcript takes the flexible top
/// region; a 1-row spinner strip sits directly above the input
/// box; the input box (editor or model picker) follows; the
/// status bar anchors the very bottom. Moving the status row
/// below the input matches pi's layout — the information that
/// "belongs" to what you're typing lives under the typing box,
/// not above it.
fn layout(area: Rect, bottom_height: u16) -> (Rect, Rect, Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(bottom_height),
            Constraint::Length(1),
        ])
        .split(area);
    (chunks[0], chunks[1], chunks[2], chunks[3])
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

fn discovery_model_api(provider_name: &str, api: ApiKind, base_url: &str) -> ApiKind {
    if is_ollama_native_discovery_target(provider_name, base_url) {
        ApiKind::OllamaChatApi
    } else {
        api
    }
}

fn discovery_model_base_url(api: ApiKind, base_url: &str) -> String {
    if api == ApiKind::OllamaChatApi {
        ollama_native_base_url(base_url)
    } else {
        base_url.to_string()
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
    state: &mut StatusBarState,
    agent_state: &AgentUiState,
    transcript_scrolled: bool,
    area: Rect,
    buf: &mut ratatui::buffer::Buffer,
) {
    let used_tokens = state
        .last_known_input_tokens
        .unwrap_or(state.estimated_context_tokens);
    // `responding...` / `compacting Ns` used to live in the
    // status bar as the leading char of the row. It moved to
    // `render_spinner_row` so the activity indicator sits
    // right above the input box (stable position, doesn't
    // jitter into and out of the persistent info row).
    let _ = agent_state;
    // Resolve the cached cwd before the format!, since the
    // shortened_cwd accessor takes &mut self and the
    // remaining state reads inside format! are immutable.
    let short_cwd = state.shortened_cwd().to_string();
    let status = format!(
        " {}{}:{} │ thinking: {} │ {}/{} │ {}",
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
        short_cwd,
    );
    Paragraph::new(Line::from(Span::styled(
        status,
        Style::default().add_modifier(Modifier::DIM),
    )))
    .render(area, buf);
}

/// Render the 1-row activity strip that sits directly above
/// the input box. Shows the active agent state — streaming,
/// tool-executing, compacting — or stays blank when idle. The
/// position is load-bearing: it's what the user's eye is
/// already anchored to (just above the thing they're typing
/// in), so the "still working" cue never moves or fights with
/// the transcript.
///
/// PR 05 of `docs/tui_polish_2026-04-26/`: replaced the braille
/// spinner glyph with a single `•` bullet that alternates
/// between Yellow and Yellow+DIM on a 600 ms period. The
/// breathing effect reads as "alive" without the rectangular-
/// blob look braille glyphs have in some monospace fonts. The
/// trailing `...` is gone from the activity strings — the
/// live spinner is the live indicator; the static dots were
/// noise.
fn render_spinner_row(
    agent_state: &AgentUiState,
    area: Rect,
    buf: &mut ratatui::buffer::Buffer,
) {
    let label: String = match agent_state {
        AgentUiState::Idle => String::new(),
        AgentUiState::Streaming => "Responding".into(),
        AgentUiState::ToolExecuting { tool_name } => format!("Running {tool_name}"),
        AgentUiState::Compacting { started_at } => {
            let elapsed = started_at.elapsed().as_secs();
            format!("compacting {elapsed}s")
        }
    };
    if label.is_empty() {
        // Still render an empty paragraph so ratatui clears
        // any previous content in this cell region on a paint.
        Paragraph::new(Line::default()).render(area, buf);
        return;
    }
    let elapsed = animation_reference().elapsed();
    let bullet = breathing_bullet(elapsed);
    let line = Line::from(vec![
        Span::raw(" "),
        bullet,
        Span::styled(format!(" {label}"), Style::default().fg(Color::Yellow)),
    ]);
    Paragraph::new(line).render(area, buf);
}

/// Process-relative reference time for live UI animations.
/// Lazily initialized on first call. Uses `OnceLock` so the
/// reference is shared across all callers without any per-app
/// plumbing.
fn animation_reference() -> &'static Instant {
    static REF: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    REF.get_or_init(Instant::now)
}

/// Return the styled `•` bullet for a given animation-elapsed
/// duration. Alternates Yellow / Yellow+DIM on a 600 ms cycle.
/// Pure function so tests can pin the cycle without touching
/// real time.
fn breathing_bullet(elapsed: Duration) -> Span<'static> {
    let phase = elapsed.as_millis() / 600 % 2;
    let style = if phase == 0 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::DIM)
    };
    Span::styled("•", style)
}

/// Map a slash-command name to the `UiAction` it forwards
/// when the command takes no arguments and has no side effects
/// other than dispatching that action. Returns `None` for any
/// command that needs custom dispatch logic (`/model`,
/// `/thinking`, `/clear`, `/quit`, etc.) — those stay in the
/// main `dispatch_validated_command` match.
///
/// Adding a new no-arg builtin: extend the table here and the
/// coverage test in `anie-cli/src/commands.rs`.
fn fixed_noarg_action(name: &str) -> Option<UiAction> {
    Some(match name {
        "compact" => UiAction::Compact,
        "fork" => UiAction::ForkSession,
        "diff" => UiAction::ShowDiff,
        "new" => UiAction::NewSession,
        "tools" => UiAction::ShowTools,
        "state" => UiAction::ShowState,
        "help" => UiAction::ShowHelp,
        "reload" => UiAction::ReloadConfig {
            provider: None,
            model: None,
        },
        _ => return None,
    })
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
            ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
            // Redacted thinking is encrypted; the UI shows a placeholder
            // rather than the opaque base64 payload.
            ContentBlock::RedactedThinking { .. } => Some("[reasoning redacted]"),
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
    join_text_blocks(&result.content)
}

fn tool_result_message_body(result: &ToolResultMessage) -> String {
    if let Some(diff) = result
        .details
        .get("diff")
        .and_then(serde_json::Value::as_str)
    {
        return diff.to_string();
    }
    join_text_blocks(&result.content)
}

/// Concatenate the `ContentBlock::Text` blocks in `content`
/// joined by newlines. Single-pass allocation: reserves the
/// exact output capacity and writes in place, rather than
/// collecting into an intermediate `Vec<&str>` + `.join()`
/// (which allocates twice — once for the Vec, once for the
/// joined String). Plan 04 PR-F / finding #52.
fn join_text_blocks(content: &[ContentBlock]) -> String {
    // First pass: compute output capacity + count the text
    // blocks so we can write the join separators exactly.
    let mut total_bytes = 0usize;
    let mut text_blocks = 0usize;
    for block in content {
        if let ContentBlock::Text { text } = block {
            if text_blocks > 0 {
                total_bytes += 1; // '\n' separator
            }
            total_bytes += text.len();
            text_blocks += 1;
        }
    }
    let mut out = String::with_capacity(total_bytes);
    let mut first = true;
    for block in content {
        if let ContentBlock::Text { text } = block {
            if !first {
                out.push('\n');
            }
            out.push_str(text);
            first = false;
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    /// PR 05 of `docs/tui_polish_2026-04-26/`: bullet alternates
    /// between Yellow and Yellow+DIM on a 600 ms cycle.
    #[test]
    fn breathing_bullet_alternates_on_600ms_period() {
        // Phase 0: t=0, t=300ms (within first half-cycle)
        let early = breathing_bullet(Duration::from_millis(0));
        let late_first_phase = breathing_bullet(Duration::from_millis(599));
        assert_eq!(early.content, "•");
        assert_eq!(late_first_phase.content, "•");
        assert!(!early.style.add_modifier.contains(Modifier::DIM));
        assert!(!late_first_phase.style.add_modifier.contains(Modifier::DIM));
        // Phase 1: t=600ms — DIM kicks in.
        let dim_phase = breathing_bullet(Duration::from_millis(600));
        assert!(dim_phase.style.add_modifier.contains(Modifier::DIM));
        // Phase 0 again at t=1200ms.
        let next_bright = breathing_bullet(Duration::from_millis(1200));
        assert!(!next_bright.style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn breathing_bullet_uses_yellow_foreground_in_both_phases() {
        let bright = breathing_bullet(Duration::from_millis(0));
        let dim = breathing_bullet(Duration::from_millis(600));
        assert_eq!(bright.style.fg, Some(Color::Yellow));
        assert_eq!(dim.style.fg, Some(Color::Yellow));
    }

    #[test]
    fn tui_model_picker_converts_ollama_discovery_to_ollama_chat_api() {
        let api = discovery_model_api(
            "ollama",
            ApiKind::OpenAICompletions,
            "http://localhost:11434/v1",
        );

        assert_eq!(api, ApiKind::OllamaChatApi);
        assert_eq!(
            discovery_model_base_url(api, "http://localhost:11434/v1"),
            "http://localhost:11434"
        );
    }
}
