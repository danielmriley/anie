# Phase 3: TUI (Weeks 5–6)

**Goal:** Build the terminal user interface. By the end of Phase 3, you should be able to launch `anie`, see the two-pane layout, type a prompt, watch the assistant's response stream in real-time, see tool calls rendered with borders and diffs, and scroll through the conversation history. Keep `anie-tui` UI-only: rendering and input live in the TUI crate, while config/auth/session/agent orchestration stay in `anie-cli`.

---

## Sub-phase 3.1: Ratatui Application Skeleton

**Duration:** Days 1–2

### Dependencies

```toml
[dependencies]
ratatui = { workspace = true }
crossterm = { workspace = true }
```

Add to workspace deps:
```toml
ratatui = "0.29"
crossterm = "0.28"
```

### Application Structure

```rust
// crates/anie-tui/src/lib.rs

pub struct App {
    // Layout state
    output_pane: OutputPane,
    status_bar: StatusBarState,
    input_pane: InputPane,

    // Agent state
    agent_state: AgentUiState,

    // Event channels
    event_rx: mpsc::Receiver<AgentEvent>,
    action_tx: mpsc::Sender<UiAction>,

    // Terminal
    should_quit: bool,
}

pub enum AgentUiState {
    Idle,
    Streaming,
    ToolExecuting { tool_name: String },
}

pub enum UiAction {
    SubmitPrompt(String),
    Abort,
    Quit,
    ScrollUp(u16),
    ScrollDown(u16),
    SelectModel,
    SetModel(String),
    SetThinking(ThinkingLevel),
    // ...
}
```

### Main Loop

The TUI main loop uses a standard ratatui pattern with three event sources:
1. Terminal key events (from crossterm).
2. Agent events (from the mpsc channel).
3. Tick events (for spinner animation).

```rust
pub async fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        // Draw
        terminal.draw(|frame| {
            app.render(frame);
        })?;

        // Handle events with timeout for animation tick
        tokio::select! {
            // Terminal input events
            Ok(true) = tokio::task::spawn_blocking(|| {
                crossterm::event::poll(Duration::from_millis(50))
            }) => {
                if let Ok(event) = crossterm::event::read() {
                    app.handle_terminal_event(event)?;
                }
            }
            // Agent events
            Some(event) = app.event_rx.recv() => {
                app.handle_agent_event(event)?;
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
```

**Important note on crossterm event polling:**
`crossterm::event::poll` and `crossterm::event::read` are blocking. They must be run on a blocking thread via `tokio::task::spawn_blocking` or a dedicated thread. Do NOT call them directly in the async runtime — they will block the executor.

**Better approach — use `crossterm::event::EventStream`:**

```rust
use crossterm::event::EventStream;
use futures::StreamExt;

let mut term_events = EventStream::new();

loop {
    tokio::select! {
        Some(Ok(event)) = term_events.next() => {
            app.handle_terminal_event(event)?;
        }
        Some(event) = app.event_rx.recv() => {
            app.handle_agent_event(event)?;
        }
        _ = tokio::time::sleep(Duration::from_millis(100)) => {
            // Animation tick
        }
    }

    terminal.draw(|frame| app.render(frame))?;

    if app.should_quit {
        break;
    }
}
```

### Terminal Setup / Teardown

```rust
pub fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
    )?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(Into::into)
}

pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
    )?;
    terminal.show_cursor()?;
    Ok(())
}
```

**Critical — panic handler:**
Install a panic hook that restores the terminal before printing the panic message. Without this, a panic leaves the terminal in raw mode and alternate screen, which is very disorienting.

```rust
pub fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // Best-effort terminal restoration
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
        );
        original_hook(panic_info);
    }));
}
```

### Acceptance Criteria

- Terminal enters alternate screen on start, exits cleanly on quit.
- Panic hook restores terminal state.
- Main loop receives both terminal and agent events.

---

## Sub-phase 3.2: Layout and Static Rendering

**Duration:** Days 2–4

### Layout Calculation

```
┌────────────────────────────────────────────┐
│  Output Pane (variable height)              │  ← fills remaining space
│                                             │
│                                             │
│                                             │
├────────────────────────────────────────────┤
│  Status Bar (1 line)                        │  ← fixed 1 line
├────────────────────────────────────────────┤
│  Input Pane (3-8 lines, dynamic)            │  ← min 3, grows with content
└────────────────────────────────────────────┘
```

```rust
fn layout(area: Rect, input_height: u16) -> (Rect, Rect, Rect) {
    let status_height = 1;
    let input_height = input_height.clamp(3, 8);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),                    // Output
            Constraint::Length(status_height),      // Status
            Constraint::Length(input_height),        // Input
        ])
        .split(area);

    (chunks[0], chunks[1], chunks[2])
}
```

### OutputPane

The output pane is a scrollable viewport rendering conversation blocks.

```rust
pub struct OutputPane {
    blocks: Vec<RenderedBlock>,
    scroll_offset: u16,
    auto_scroll: bool,  // true when at bottom
}

pub enum RenderedBlock {
    UserMessage {
        text: String,
        timestamp: u64,
    },
    AssistantMessage {
        text: String,
        is_streaming: bool,
        timestamp: u64,
    },
    ToolCall {
        tool_name: String,
        args_display: String,
        result: Option<ToolCallResult>,
        is_executing: bool,
    },
}

pub struct ToolCallResult {
    pub content: String,
    pub is_error: bool,
    pub elapsed: Option<Duration>,
}
```

**Rendering each block type:**

1. **User message:** Prefixed with `> You:` in a distinct color. Wrap text to terminal width.
2. **Assistant message:** No prefix. Render as plain text for now (markdown rendering comes later as polish).
3. **Tool call:** Bordered box with tool name as title:

```
┌─ read ──────────────────────────────────────┐
│ path: src/main.rs                           │
│ [42 lines, 1.2 KB]                          │
└─────────────────────────────────────────────┘
```

**Scrolling:**
- Track `scroll_offset` as number of lines from the top.
- `auto_scroll = true` when the user is at the bottom. New content auto-scrolls.
- If the user scrolls up, `auto_scroll = false`. New content does NOT scroll.
- Scrolling back to the bottom re-enables auto-scroll.

```rust
impl OutputPane {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let total_lines = self.compute_total_lines(area.width);

        // If auto-scroll, adjust offset to show bottom
        let scroll_offset = if self.auto_scroll {
            total_lines.saturating_sub(area.height)
        } else {
            self.scroll_offset
        };

        // Render only visible lines
        let visible_lines = self.get_lines(scroll_offset, area.height, area.width);
        for (i, line) in visible_lines.iter().enumerate() {
            buf.set_line(area.x, area.y + i as u16, line, area.width);
        }
    }
}
```

### StatusBar

Single line showing:
```
model: claude-sonnet-4-6 │ thinking: medium │ 12.4k/200k │ ~/Projects/myproject
```

```rust
pub struct StatusBarState {
    pub model_name: String,
    pub thinking: ThinkingLevel,
    pub last_known_input_tokens: Option<u64>,
    pub estimated_context_tokens: u64,
    pub context_window: u64,
    pub cwd: String,
}

fn render_status_bar(state: &StatusBarState, area: Rect, buf: &mut Buffer) {
    let used_tokens = state
        .last_known_input_tokens
        .unwrap_or(state.estimated_context_tokens);

    let status = format!(
        " model: {} │ thinking: {:?} │ {}/{} │ {}",
        state.model_name,
        state.thinking,
        format_tokens(used_tokens),
        format_tokens(state.context_window),
        shorten_path(&state.cwd),
    );

    let line = Line::from(vec![
        Span::styled(status, Style::default().fg(Color::DarkGray)),
    ]);

    buf.set_line(area.x, area.y, &line, area.width);
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{}", tokens)
    }
}
```

### InputPane

A multi-line text editor for composing messages.

```rust
pub struct InputPane {
    content: String,
    cursor_pos: usize,       // byte offset in content
    cursor_line: usize,      // visual line
    cursor_col: usize,       // visual column
    history: Vec<String>,
    history_index: Option<usize>,
}
```

**Rendering:**
- Show the content with a blinking cursor.
- Prefix with `> ` on the first line.
- Wrap long lines to terminal width.

**Input handling:**
- Characters: insert at cursor position.
- Backspace: delete before cursor.
- Delete: delete after cursor.
- Arrow keys: move cursor.
- Home/End: move to start/end of line.
- Ctrl+A/E: move to start/end of line.
- Ctrl+W: delete word backward.
- Ctrl+K: delete to end of line.
- Ctrl+U: delete entire line.
- Alt+Left/Right: move by word.

### Acceptance Criteria

- Three-pane layout renders correctly at various terminal sizes.
- Static user messages and assistant messages display.
- Status bar shows model info.
- Input pane accepts text input and handles cursor movement.
- Scrolling works with Page Up/Down.
- `ratatui::TestBackend` snapshot tests cover at least `OutputPane`, `StatusBar`, and one wrapped `InputPane` case.

---

## Sub-phase 3.3: Key Bindings and Input Handling

**Duration:** Days 3–4

### Key Map

```rust
fn handle_key_event(&mut self, key: KeyEvent) -> Result<()> {
    match self.agent_state {
        AgentUiState::Idle => self.handle_idle_input(key),
        AgentUiState::Streaming | AgentUiState::ToolExecuting { .. } => {
            self.handle_active_input(key)
        }
    }
}

fn handle_idle_input(&mut self, key: KeyEvent) -> Result<()> {
    match (key.modifiers, key.code) {
        // Submit
        (KeyModifiers::NONE, KeyCode::Enter) => {
            let text = self.input_pane.take_content();
            if !text.trim().is_empty() {
                self.handle_submit(text);
            }
        }
        // Multiline
        (KeyModifiers::SHIFT, KeyCode::Enter) | (KeyModifiers::ALT, KeyCode::Enter) => {
            self.input_pane.insert_newline();
        }
        // Quit
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
            self.should_quit = true;
        }
        // History
        (KeyModifiers::NONE, KeyCode::Up) => {
            self.input_pane.history_previous();
        }
        (KeyModifiers::NONE, KeyCode::Down) => {
            self.input_pane.history_next();
        }
        // Scrolling
        (KeyModifiers::NONE, KeyCode::PageUp) => {
            self.output_pane.scroll_up(self.output_area_height);
        }
        (KeyModifiers::NONE, KeyCode::PageDown) => {
            self.output_pane.scroll_down(self.output_area_height);
        }
        // Model selector
        (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
            self.open_model_selector();
        }
        // Clear
        (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
            self.output_pane.clear();
        }
        // Editing keys
        _ => self.input_pane.handle_key(key),
    }
    Ok(())
}

fn handle_active_input(&mut self, key: KeyEvent) -> Result<()> {
    match (key.modifiers, key.code) {
        // Interrupt
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            self.abort_agent();
        }
        // Quit (double Ctrl+C)
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
            self.should_quit = true;
        }
        // Allow scrolling during agent execution
        (KeyModifiers::NONE, KeyCode::PageUp) => {
            self.output_pane.scroll_up(self.output_area_height);
        }
        (KeyModifiers::NONE, KeyCode::PageDown) => {
            self.output_pane.scroll_down(self.output_area_height);
        }
        _ => {}
    }
    Ok(())
}
```

### Input History

```rust
impl InputPane {
    pub fn history_previous(&mut self) {
        if self.history.is_empty() { return; }
        match self.history_index {
            None => {
                // Save current content, show last history item
                self.saved_content = Some(self.content.clone());
                self.history_index = Some(self.history.len() - 1);
            }
            Some(0) => return,  // Already at oldest
            Some(i) => {
                self.history_index = Some(i - 1);
            }
        }
        if let Some(i) = self.history_index {
            self.content = self.history[i].clone();
            self.cursor_pos = self.content.len();
        }
    }

    pub fn history_next(&mut self) {
        match self.history_index {
            None => return,
            Some(i) if i >= self.history.len() - 1 => {
                // Restore saved content
                self.history_index = None;
                if let Some(saved) = self.saved_content.take() {
                    self.content = saved;
                    self.cursor_pos = self.content.len();
                }
            }
            Some(i) => {
                self.history_index = Some(i + 1);
                self.content = self.history[i + 1].clone();
                self.cursor_pos = self.content.len();
            }
        }
    }
}
```

### Acceptance Criteria

- Enter submits, Shift+Enter inserts newline.
- Ctrl+C aborts the agent.
- Up/Down navigates input history.
- Page Up/Down scrolls the output.
- All editing keys (Ctrl+A/E/W/K/U, Alt+arrows) work.

---

## Sub-phase 3.4: Streaming Integration

**Duration:** Days 4–6

This is where the TUI comes alive. Connect `AgentEvent` to the output pane for real-time rendering.

### Event Handler

```rust
fn handle_agent_event(&mut self, event: AgentEvent) -> Result<()> {
    match event {
        AgentEvent::AgentStart => {
            self.agent_state = AgentUiState::Streaming;
        }

        AgentEvent::MessageStart { message } => {
            match &message {
                Message::User(um) => {
                    self.output_pane.add_block(RenderedBlock::UserMessage {
                        text: um.content.iter()
                            .filter_map(|c| match c {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        timestamp: um.timestamp,
                    });
                }
                Message::Assistant(_) => {
                    self.output_pane.add_block(RenderedBlock::AssistantMessage {
                        text: String::new(),
                        is_streaming: true,
                        timestamp: 0,
                    });
                }
                _ => {}
            }
        }

        AgentEvent::MessageDelta { delta } => {
            match delta {
                StreamDelta::TextDelta(text) => {
                    self.output_pane.append_to_last_assistant(&text);
                }
                StreamDelta::ThinkingDelta(text) => {
                    // For v1, we can either show thinking inline (dimmed)
                    // or collapse it. Start with inline dimmed.
                    self.output_pane.append_thinking_to_last_assistant(&text);
                }
                _ => {}
            }
        }

        AgentEvent::MessageEnd { message } => {
            if let Message::Assistant(am) = &message {
                self.output_pane.finalize_last_assistant(am);
                self.status_bar.last_known_input_tokens = Some(am.usage.input_tokens);
            }
        }

        AgentEvent::ToolExecStart { call_id, tool_name, args } => {
            self.agent_state = AgentUiState::ToolExecuting {
                tool_name: tool_name.clone(),
            };
            self.output_pane.add_block(RenderedBlock::ToolCall {
                tool_name,
                args_display: format_tool_args(&args),
                result: None,
                is_executing: true,
            });
        }

        AgentEvent::ToolExecUpdate { call_id, partial } => {
            self.output_pane.update_tool_result(&call_id, &partial, true);
        }

        AgentEvent::ToolExecEnd { call_id, result, is_error } => {
            self.output_pane.finalize_tool_result(&call_id, &result, is_error);
            self.agent_state = AgentUiState::Streaming;
        }

        AgentEvent::TurnEnd { .. } => {
            // Turn completed, might loop for more tool calls
        }

        AgentEvent::AgentEnd { messages } => {
            self.agent_state = AgentUiState::Idle;
        }

        _ => {}
    }
    Ok(())
}
```

### Streaming Text Append

The key performance challenge: appending streaming text to the output pane without re-rendering the entire conversation.

```rust
impl OutputPane {
    pub fn append_to_last_assistant(&mut self, text: &str) {
        if let Some(RenderedBlock::AssistantMessage { text: ref mut content, .. }) =
            self.blocks.last_mut()
        {
            content.push_str(text);
            // Invalidate cached line count for this block
            self.cached_lines_dirty = true;
            // Auto-scroll if at bottom
            if self.auto_scroll {
                self.scroll_to_bottom();
            }
        }
    }
}
```

**Performance note:** During streaming, the TUI redraws at 30-60 fps. Each redraw computes the visible lines for the output pane. For the streaming block (the last assistant message), cache the line wrapping and only recompute from the point where new text was appended. This avoids O(n) line wrapping on every frame for long messages.

### Spinner Animation

Show a spinner next to the status bar or inline when the agent is working:

```rust
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Spinner {
    frame: usize,
    last_tick: Instant,
}

impl Spinner {
    pub fn tick(&mut self) -> &str {
        if self.last_tick.elapsed() >= Duration::from_millis(80) {
            self.frame = (self.frame + 1) % SPINNER_FRAMES.len();
            self.last_tick = Instant::now();
        }
        SPINNER_FRAMES[self.frame]
    }
}
```

### Acceptance Criteria

- Assistant text streams into the output pane character by character.
- Tool calls appear with a bordered box showing the tool name and arguments.
- Tool results appear inside the bordered box when complete.
- Spinner animates during streaming and tool execution.
- Auto-scroll follows new content. Manual scrolling disables auto-scroll.
- Event-to-render tests cover streaming deltas and the tool-call lifecycle.

---

## Sub-phase 3.5: Tool Call Rendering

**Duration:** Days 5–7

### Tool Call Display Format

Each tool call is rendered as a bordered block:

```
┌─ read ──────────────────────────────────────┐
│ path: src/main.rs                           │
│ [42 lines, 1.2 KB]                          │
└─────────────────────────────────────────────┘
```

For bash:
```
┌─ $ echo hello world ────────────────────────┐
│ hello world                                  │
│                                              │
│ Took 0.1s                                    │
└──────────────────────────────────────────────┘
```

For edit (with diff):
```
┌─ edit src/main.rs ──────────────────────────┐
│ -    let x = foo();                          │
│ +    let x = foo().unwrap_or_default();      │
└──────────────────────────────────────────────┘
```

### Rendering Implementation

```rust
fn render_tool_call(block: &RenderedBlock, area: Rect, buf: &mut Buffer) {
    if let RenderedBlock::ToolCall { tool_name, args_display, result, is_executing } = block {
        // Title line
        let title = format!(" {} ", format_tool_title(tool_name, args_display));

        // Border
        let border_style = if *is_executing {
            Style::default().fg(Color::Yellow)
        } else if result.as_ref().map_or(false, |r| r.is_error) {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let block_widget = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(title);

        let inner = block_widget.inner(area);
        block_widget.render(area, buf);

        // Content
        if let Some(result) = result {
            let content_style = if result.is_error {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            // Truncate display to available height
            let lines: Vec<&str> = result.content.lines().take(inner.height as usize).collect();
            for (i, line) in lines.iter().enumerate() {
                let span = Span::styled(*line, content_style);
                buf.set_span(inner.x, inner.y + i as u16, &span, inner.width);
            }
        } else if *is_executing {
            // Show spinner
            let spinner_text = format!("{} executing...", spinner.tick());
            let span = Span::styled(spinner_text, Style::default().fg(Color::Yellow));
            buf.set_span(inner.x, inner.y, &span, inner.width);
        }
    }
}
```

### Diff Rendering

For `edit` tool results, render a colored diff:

```rust
fn render_diff(diff_text: &str, area: Rect, buf: &mut Buffer) {
    for (i, line) in diff_text.lines().enumerate().take(area.height as usize) {
        let style = if line.starts_with('+') {
            Style::default().fg(Color::Green)
        } else if line.starts_with('-') {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let span = Span::styled(line, style);
        buf.set_span(area.x, area.y + i as u16, &span, area.width);
    }
}
```

### Tool-Specific Formatters

```rust
fn format_tool_title(tool_name: &str, args_display: &str) -> String {
    match tool_name {
        "bash" => {
            // Show the command as the title
            format!("$ {}", args_display)
        }
        "read" | "write" | "edit" => {
            // Show tool name + path
            format!("{} {}", tool_name, args_display)
        }
        _ => {
            format!("{}", tool_name)
        }
    }
}

fn format_tool_args(args: &serde_json::Value) -> String {
    // Extract the most relevant argument for display
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        return path.to_string();
    }
    if let Some(command) = args.get("command").and_then(|v| v.as_str()) {
        // Truncate long commands
        if command.len() > 60 {
            return format!("{}...", &command[..57]);
        }
        return command.to_string();
    }
    // Fallback: pretty-print args
    serde_json::to_string(args).unwrap_or_default()
}
```

### Acceptance Criteria

- Read tool shows file path and size summary.
- Bash tool shows command as title, output in body, elapsed time.
- Edit tool shows diff with color-coded added/removed lines.
- Error results are shown in red.
- Executing tools show a spinner.

---

## Sub-phase 3.6: Interactive Orchestration (`anie-cli` + `anie-tui`)

**Duration:** Days 6–8

Keep the TUI crate focused on rendering and input. Config loading, provider/tool/session setup, compaction checks, and agent-task orchestration live in `anie-cli` (for example in `interactive_mode.rs`). The TUI emits `UiAction`s and renders `AgentEvent`s; it does **not** own config or the canonical conversation context.

### Startup Sequence

```rust
pub async fn start_interactive(cli_args: CliArgs) -> Result<()> {
    install_panic_hook();

    // 1. Load config + session/controller state in CLI land
    let config = load_config(cli_args.into_overrides())?;
    let cwd = std::env::current_dir()?;

    // 2. Set up providers
    let mut provider_registry = ProviderRegistry::new();
    register_builtin_providers(&mut provider_registry);
    let provider_registry = Arc::new(provider_registry);

    // 3. Set up tools
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register(Arc::new(ReadTool::new(cwd.to_str().unwrap())));
    tool_registry.register(Arc::new(WriteTool::new(cwd.to_str().unwrap())));
    tool_registry.register(Arc::new(BashTool::new(cwd.to_str().unwrap())));
    let tool_registry = Arc::new(tool_registry);

    // 4. Resolve model + request resolver
    let model = resolve_model(&config)?;
    let request_options_resolver = Arc::new(AuthResolver {
        cli_api_key: cli_args.api_key.clone(),
        config: config.clone(),
    });

    // 5. Build system prompt
    let system_prompt = build_system_prompt(&cwd, &tool_registry, &config)?;

    // 6. Create agent loop
    let agent_loop = Arc::new(AgentLoop::new(
        provider_registry,
        tool_registry,
        AgentLoopConfig {
            model,
            system_prompt,
            thinking: config.model.thinking,
            tool_execution: ToolExecutionMode::Parallel,
            request_options_resolver,
            get_steering_messages: None,
            get_follow_up_messages: None,
        },
    ));

    // 7. Split UI actions from agent events
    let (agent_event_tx, agent_event_rx) = mpsc::channel(256);
    let (ui_action_tx, ui_action_rx) = mpsc::channel(64);

    // 8. Controller owns canonical context + session persistence
    let controller = InteractiveController::new(
        agent_loop,
        ui_action_rx,
        agent_event_tx,
        config,
        cwd,
    );
    tokio::spawn(controller.run());

    // 9. TUI owns rendering only
    let mut app = App::new(agent_event_rx, ui_action_tx);
    let mut terminal = setup_terminal()?;
    let result = run_tui(&mut terminal, &mut app).await;
    restore_terminal(&mut terminal)?;
    result
}
```

### Prompt Submission

When the user presses Enter, the TUI sends a `UiAction::SubmitPrompt(text)` to the controller. The controller owns context and session persistence:

```rust
match action {
    UiAction::SubmitPrompt(text) => {
        let user_msg = Message::User(UserMessage {
            content: vec![ContentBlock::Text { text }],
            timestamp: now_millis(),
        });

        self.session.append_message(&user_msg)?; // persist prompt immediately

        let run_result = self.agent.run(
            vec![user_msg],
            self.context.clone(),
            self.agent_event_tx.clone(),
            self.cancel.child_token(),
        ).await;

        self.session.append_messages(&run_result.generated_messages)?;
        self.context = run_result.final_context;
    }
    UiAction::Abort => self.cancel.cancel(),
    _ => {}
}
```

This removes the shared `&mut Vec<Message>` problem completely. The TUI display state is derived from events; the controller owns the canonical context.

### System Prompt Construction

```rust
pub fn build_system_prompt(
    cwd: &Path,
    tools: &ToolRegistry,
    config: &AnieConfig,
) -> Result<String> {
    let tool_list = tools.definitions().iter()
        .map(|t| format!("- {}: {}", t.name, t.description))
        .collect::<Vec<_>>()
        .join("\n");

    let default_base = format!(
        "You are an expert coding assistant. You help users by reading files, executing commands, editing code, and writing new files.\n\nAvailable tools:\n{}\n\nGuidelines:\n- Use bash for file operations like ls, grep, find\n- Use read to examine files (use offset + limit for large files)\n- Use edit for precise changes\n- Use write only for new files or complete rewrites\n- Be concise in your responses",
        tool_list,
    );

    let mut parts = vec![load_system_base(cwd)?.unwrap_or(default_base)];

    if let Some(append) = load_append_system(cwd)? {
        parts.push(append);
    }

    for ctx in collect_context_files(cwd, &config.context)? {
        parts.push(format!("# Project Context\n\n## {}\n\n{}", ctx.path.display(), ctx.contents));
    }

    parts.push(format!("Current date: {}", current_date_ymd()?));
    parts.push(format!("Current working directory: {}", cwd.display()));

    Ok(parts.join("\n\n"))
}
```

`collect_context_files()` should walk upward, merge all matching files, and enforce `config.context.max_file_bytes` plus `config.context.max_total_bytes` so large `AGENTS.md` files cannot silently consume the entire prompt.

### Acceptance Criteria

- User can type a prompt, press Enter, and see a streaming response.
- Tool calls are executed and results displayed.
- Multiple turns work (tool call → result → next response → done).
- Ctrl+C cancels the running agent.
- The TUI doesn't hang or crash during normal operation.
- `anie-tui` does not directly load config/auth or spawn provider requests; orchestration stays in `anie-cli`.

---

## Sub-phase 3.7: Slash Commands (Basic)

**Duration:** Days 7–8

Implement a minimal set of slash commands for essential operations.

### Command Detection

```rust
fn handle_submit(&mut self, text: String) {
    let trimmed = text.trim();
    if trimmed.starts_with('/') {
        self.handle_slash_command(trimmed);
    } else {
        let _ = self.action_tx.try_send(UiAction::SubmitPrompt(text));
    }
}

fn handle_slash_command(&mut self, command: &str) {
    let parts: Vec<&str> = command.splitn(2, ' ').collect();
    let cmd = parts[0];
    let arg = parts.get(1).map(|s| s.trim());

    match cmd {
        "/model" => self.handle_model_command(arg),
        "/thinking" => self.handle_thinking_command(arg),
        "/clear" => self.output_pane.clear(),
        "/help" => self.show_help(),
        "/quit" | "/exit" => self.should_quit = true,
        _ => {
            self.output_pane.add_system_message(
                &format!("Unknown command: {}. Type /help for available commands.", cmd)
            );
        }
    }
}
```

### `/model` Command

```rust
fn handle_model_command(&mut self, arg: Option<&str>) {
    match arg {
        None => {
            self.output_pane.add_system_message(
                &format!("Current model: {}", self.status_bar.model_name)
            );
        }
        Some(model_id) => {
            let _ = self.action_tx.try_send(UiAction::SetModel(model_id.to_string()));
            self.output_pane.add_system_message(
                &format!("Requested model switch: {}", model_id)
            );
        }
    }
}
```

State-changing slash commands are routed through the interactive controller. The TUI can display optimistic feedback, but the controller remains the source of truth for the active model and thinking level.

### `/help` Output

```
Available commands:
  /model [id]      — Show or switch the active model
  /thinking [level] — Set thinking level (off, low, medium, high)
  /clear           — Clear the output pane
  /help            — Show this help
  /quit            — Exit anie
```

### Acceptance Criteria

- `/model` shows the current model and routes model-switch requests to the controller.
- `/thinking` routes thinking changes to the controller.
- `/clear` clears the output.
- `/help` shows help text.
- Unknown commands show an error message.

---

## Phase 3 Milestones Checklist

| # | Milestone | Verified By |
|---|---|---|
| 1 | Terminal enters/exits alternate screen cleanly | Manual test |
| 2 | Static layout has `TestBackend` snapshot coverage | Unit tests |
| 3 | Three-pane layout renders at various sizes | Manual test + snapshots |
| 4 | Text input with editing keys works | Manual test |
| 5 | Streaming text appears in real-time | Manual test against API |
| 6 | Tool calls render with bordered blocks | Manual test |
| 7 | Diff rendering for edit results (color coded) | Manual test |
| 8 | Scrolling (Page Up/Down, auto-scroll) works | Manual test |
| 9 | Input history (Up/Down) works | Manual test |
| 10 | Ctrl+C cancels the agent | Manual test |
| 11 | Slash commands (/model, /thinking, /clear, /help) work | Manual test |
| 12 | UI crate remains orchestration-free (`anie-cli` owns controller state) | Code review |
