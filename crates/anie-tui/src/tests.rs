use tokio::sync::mpsc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Rect};

use anie_protocol::{
    AgentEvent, AssistantMessage, ContentBlock, Message, StreamDelta, Usage, UserMessage,
};
use anie_provider::{ApiKind, CostPerMillion, Model, ModelCompat};

use crate::{
    AgentUiState, App, OutputPane, RenderedBlock,
    app::{MAX_AGENT_EVENTS_PER_FRAME, drain_agent_event_batch},
    commands::{ArgumentSpec, SlashCommandInfo},
};

/// A test-only catalog that mirrors the `anie-cli` builtins so
/// tests which dispatch real slash commands have a valid shape to
/// validate against. Tests that don't exercise `/command` input
/// can safely pass `Vec::new()`.
///
/// Kept deliberately duplicated — the CLI crate can't be imported
/// from inside `anie-tui` tests, and the coupling is exercised by
/// `registry_covers_every_dispatched_slash_command` in
/// `anie-cli/src/commands.rs`.
fn default_test_commands() -> Vec<SlashCommandInfo> {
    const LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high"];
    const SESSION_SUBS: &[&str] = &["list"];
    vec![
        SlashCommandInfo::builtin_with_args(
            "model",
            "Select model",
            ArgumentSpec::FreeForm { required: false },
            Some("[<provider:id>|<id>]"),
        ),
        SlashCommandInfo::builtin_with_args(
            "thinking",
            "Set reasoning effort",
            ArgumentSpec::Enumerated {
                values: LEVELS,
                required: false,
            },
            Some("[off|minimal|low|medium|high]"),
        ),
        SlashCommandInfo::builtin_with_args(
            "context-length",
            "Query or override Ollama context length",
            ArgumentSpec::ContextLengthOverride,
            Some("[N|reset]"),
        ),
        SlashCommandInfo::builtin("compact", "Manually compact"),
        SlashCommandInfo::builtin("fork", "Fork session"),
        SlashCommandInfo::builtin("diff", "Show diff"),
        SlashCommandInfo::builtin("new", "New session"),
        SlashCommandInfo::builtin_with_args(
            "session",
            "Session info",
            ArgumentSpec::Subcommands {
                known: SESSION_SUBS,
            },
            Some("[list|<id>]"),
        ),
        SlashCommandInfo::builtin("tools", "List tools"),
        SlashCommandInfo::builtin("onboard", "Onboarding"),
        SlashCommandInfo::builtin("providers", "Manage providers"),
        SlashCommandInfo::builtin("clear", "Clear output"),
        SlashCommandInfo::builtin("reload", "Reload config"),
        SlashCommandInfo::builtin("copy", "Copy last assistant"),
        SlashCommandInfo::builtin("help", "Show help"),
        SlashCommandInfo::builtin("quit", "Quit anie"),
    ]
}

fn sample_models() -> Vec<Model> {
    vec![Model {
        id: "qwen3:32b".into(),
        name: "Qwen 3 32B".into(),
        provider: "ollama".into(),
        api: ApiKind::OpenAICompletions,
        base_url: "http://localhost:11434/v1".into(),
        context_window: 32_768,
        max_tokens: 8_192,
        supports_reasoning: true,
        reasoning_capabilities: None,
        supports_images: false,
        cost_per_million: CostPerMillion::zero(),
        replay_capabilities: None,
        compat: ModelCompat::None,
    }]
}

#[test]
fn static_layout_renders_output_status_and_input() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    {
        let status = app.status_bar_mut();
        status.provider_name = "anthropic".into();
        status.model_name = "claude-sonnet-4-6".into();
        status.thinking = "medium".into();
        status.estimated_context_tokens = 12_400;
        status.context_window = 200_000;
        status.cwd = "~/Projects/myproject".into();
    }
    app.handle_agent_event(AgentEvent::MessageStart {
        message: Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "Fix the bug in main.rs".into(),
            }],
            timestamp: 1,
        }),
    })
    .expect("handle user message");

    let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());

    assert!(screen.contains("› Fix the bug in main.rs"));
    assert!(screen.contains("anthropic:claude-sonnet-4-6 │ thinking: medium │ 12.4k/200k"));
    // `render_to_string` trims trailing spaces per line, so the
    // `> ` input prompt lands with the space stripped — anchor
    // to `\n>` to prove the marker shows up on its own line.
    assert!(
        screen.contains("\n>"),
        "input prompt missing, screen was:\n{screen}"
    );
}

#[test]
fn compaction_start_transitions_to_compacting_state() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());

    app.handle_agent_event(AgentEvent::CompactionStart)
        .expect("handle");

    assert!(
        matches!(app.agent_state(), AgentUiState::Compacting { .. }),
        "expected Compacting, got {:?}",
        app.agent_state()
    );
    // Permanent transcript record remains.
    assert!(
        app.output_blocks()
            .iter()
            .any(|block| matches!(block, RenderedBlock::SystemMessage { text } if text.contains("Compacting")))
    );
}

#[test]
fn compaction_end_transitions_back_to_idle() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());

    app.handle_agent_event(AgentEvent::CompactionStart)
        .expect("start");
    app.handle_agent_event(AgentEvent::CompactionEnd {
        summary: "Prior conversation covered setup work.".into(),
        tokens_before: 150_000,
        tokens_after: 8_000,
    })
    .expect("end");

    assert!(matches!(app.agent_state(), AgentUiState::Idle));
}

#[test]
fn status_bar_shows_elapsed_seconds_while_compacting() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    {
        let status = app.status_bar_mut();
        status.provider_name = "github-copilot".into();
        status.model_name = "claude-sonnet-4.6".into();
        status.thinking = "medium".into();
        status.estimated_context_tokens = 150_000;
        status.context_window = 200_000;
        status.cwd = "~/project".into();
    }
    app.handle_agent_event(AgentEvent::CompactionStart)
        .expect("start");

    let mut terminal = Terminal::new(TestBackend::new(120, 20)).expect("test terminal");
    terminal.draw(|frame| app.render(frame)).expect("draw");
    let screen = render_to_string(terminal.backend());

    assert!(
        screen.contains("compacting"),
        "status bar should show 'compacting': {screen}"
    );
    // 0s immediately after CompactionStart (Instant just set).
    assert!(
        screen.contains("compacting 0s"),
        "expected '0s' suffix: {screen}"
    );
}

#[test]
fn shift_modified_characters_are_inserted() {
    use crate::InputPane;

    let mut input = InputPane::new();
    input.handle_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT));
    input.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
    input.handle_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT));
    assert_eq!(input.content(), "Hi!");
}

#[test]
fn wrapped_input_snapshot_is_stable() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    for ch in "This is a very long line that should wrap inside the input pane".chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type input");
    }

    let mut terminal = Terminal::new(TestBackend::new(30, 10)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());

    assert!(screen.contains("> This is a very long line"));
    assert!(screen.contains("should wrap"));
}

#[test]
fn replayed_assistant_renders_thinking_above_visible_response() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    app.load_transcript(&[Message::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Thinking {
                thinking: "plan first".into(),
                signature: None,
            },
            ContentBlock::Text {
                text: "final answer".into(),
            },
        ],
        usage: Usage::default(),
        stop_reason: anie_protocol::StopReason::Stop,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 1,
        reasoning_details: None,
    })]);

    let mut terminal = Terminal::new(TestBackend::new(60, 12)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());
    let thinking_index = screen
        .find("• Thinking\n  └ plan first")
        .expect("thinking section");
    let text_index = screen.find("final answer").expect("visible answer");

    assert!(thinking_index < text_index, "screen was:\n{screen}");
    assert!(
        screen.contains("• Thinking\n  └ plan first\n\nfinal answer"),
        "screen was:\n{screen}"
    );
}

#[test]
fn streaming_assistant_renders_thinking_above_visible_response() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());

    app.handle_agent_event(AgentEvent::AgentStart)
        .expect("agent start");
    app.handle_agent_event(AgentEvent::MessageStart {
        message: Message::Assistant(AssistantMessage {
            content: vec![],
            usage: Usage::default(),
            stop_reason: anie_protocol::StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock-model".into(),
            timestamp: 1,
            reasoning_details: None,
        }),
    })
    .expect("assistant start");
    app.handle_agent_event(AgentEvent::MessageDelta {
        delta: StreamDelta::TextDelta("final answer".into()),
    })
    .expect("text delta");
    app.handle_agent_event(AgentEvent::MessageDelta {
        delta: StreamDelta::ThinkingDelta("plan first".into()),
    })
    .expect("thinking delta");

    let mut terminal = Terminal::new(TestBackend::new(60, 16)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());
    let thinking_index = screen
        .find("• Thinking\n  └ plan first")
        .expect("thinking section");
    let text_index = screen.find("final answer").expect("visible answer");
    // The streaming activity indicator moved out of the
    // transcript and onto a fixed row above the input box —
    // `render_spinner_row` renders `• Responding` there.
    // PR 05 of `docs/tui_polish_2026-04-26/` dropped the
    // trailing `...` and replaced the braille spinner with a
    // breathing `•`.
    let streaming_index = screen.find("Responding").expect("streaming status");

    assert!(thinking_index < text_index, "screen was:\n{screen}");
    assert!(text_index < streaming_index, "screen was:\n{screen}");
    assert!(
        screen.contains("• Thinking\n  └ plan first\n\nfinal answer"),
        "screen was:\n{screen}"
    );
}

#[test]
fn urgent_input_render_reuses_existing_output_snapshot() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());

    app.handle_agent_event(AgentEvent::SystemMessage {
        text: "existing transcript".into(),
    })
    .expect("seed transcript");

    let mut terminal = Terminal::new(TestBackend::new(60, 16)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("initial draw");
    let builds_before = app.output_flat_build_count();

    app.handle_agent_event(AgentEvent::SystemMessage {
        text: "pending transcript update".into(),
    })
    .expect("queue transcript update");
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
    )))
    .expect("type input");
    terminal
        .draw(|frame| app.render_urgent(frame))
        .expect("urgent draw");
    let screen = render_to_string(terminal.backend());

    assert_eq!(
        app.output_flat_build_count(),
        builds_before,
        "urgent input paint should reuse the previous output snapshot"
    );
    assert_eq!(app.input_pane_contents(), "x");
    assert!(
        screen.contains("> x"),
        "urgent input paint should still show the typed input, screen was:\n{screen}"
    );
    assert!(
        !screen.contains("pending transcript update"),
        "urgent input paint should reuse the previous transcript snapshot, screen was:\n{screen}"
    );
}

#[test]
fn wrapped_thinking_lines_keep_their_section_gutter() {
    let screen = render_assistant_block("done", "abcdefghijklmnop", false, 14, 8);

    // First body line uses the `  └ ` indent; continuations
    // use `    ` (four spaces) — both consume 4 chars, so at
    // width=14 the body wraps to 10-char slices.
    assert!(
        screen.contains("• Thinking\n  └ abcdefghij\n    klmnop\n\ndone"),
        "screen was:\n{screen}"
    );
}

#[test]
fn answer_only_assistant_rendering_remains_plain() {
    let screen = render_assistant_block("final answer", "", false, 20, 6);

    assert_eq!(non_empty_lines(&screen), vec!["final answer"]);
}

#[test]
fn thinking_only_assistant_rendering_remains_grouped() {
    let screen = render_assistant_block("", "plan first", false, 20, 6);

    assert_eq!(
        non_empty_lines(&screen),
        vec!["• Thinking", "  └ plan first"]
    );
}

#[test]
fn streaming_assistant_without_visible_answer_reports_thinking_status() {
    let screen = render_assistant_block("", "plan first", true, 20, 6);

    // While thinking is streaming with no visible answer yet,
    // the `•` bullet swaps to the animated spinner frame — the
    // header itself is the "still thinking" indicator, so the
    // old separate `⠋ thinking...` status line is redundant.
    assert!(
        screen.contains("⠋ Thinking\n  └ plan first"),
        "screen was:\n{screen}"
    );
    assert!(!screen.contains("responding..."), "screen was:\n{screen}");
}

#[test]
fn empty_streaming_assistant_block_renders_blank_in_output_pane() {
    // The "⠋ streaming..." indicator moved out of the
    // assistant block and onto the dedicated spinner row
    // above the input box (rendered by `render_spinner_row`
    // in `app.rs`). A streaming assistant block with no
    // content yet therefore contributes nothing to the
    // transcript — the spinner row is the sole "still
    // working" cue.
    let screen = render_assistant_block("", "", true, 20, 4);
    assert!(
        non_empty_lines(&screen).is_empty(),
        "expected empty output pane, got: {screen}"
    );
}

#[test]
fn event_to_render_streaming_and_tool_lifecycle() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());

    app.handle_agent_event(AgentEvent::AgentStart)
        .expect("agent start");
    app.handle_agent_event(AgentEvent::MessageStart {
        message: Message::Assistant(AssistantMessage {
            content: vec![],
            usage: Usage::default(),
            stop_reason: anie_protocol::StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock-model".into(),
            timestamp: 1,
            reasoning_details: None,
        }),
    })
    .expect("assistant start");
    app.handle_agent_event(AgentEvent::MessageDelta {
        delta: StreamDelta::TextDelta("I'll read the file first.".into()),
    })
    .expect("assistant delta");
    app.handle_agent_event(AgentEvent::ToolExecStart {
        call_id: "call_1".into(),
        tool_name: "read".into(),
        args: serde_json::json!({"path": "src/main.rs"}),
    })
    .expect("tool start");
    app.handle_agent_event(AgentEvent::ToolExecEnd {
        call_id: "call_1".into(),
        result: anie_protocol::ToolResult {
            content: vec![ContentBlock::Text {
                text: "fn main() {}".into(),
            }],
            details: serde_json::json!({}),
        },
        is_error: false,
    })
    .expect("tool end");
    app.handle_agent_event(AgentEvent::AgentEnd { messages: vec![] })
        .expect("agent end");

    let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());

    assert!(screen.contains("I'll read the file first."));
    assert!(
        screen.contains("• Read src/main.rs"),
        "tool header missing, screen was:\n{screen}"
    );
    assert!(screen.contains("fn main() {}"));
}

#[test]
fn ctrl_c_marks_abort_while_active() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    app.handle_agent_event(AgentEvent::AgentStart)
        .expect("agent start");
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )))
    .expect("ctrl-c");
    let action = action_rx.try_recv().expect("abort action");
    assert!(matches!(action, crate::UiAction::Abort));
}

#[test]
fn second_ctrl_c_while_active_quits() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    app.handle_agent_event(AgentEvent::AgentStart)
        .expect("agent start");
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )))
    .expect("first ctrl-c");
    assert!(matches!(
        action_rx.try_recv().expect("abort action"),
        crate::UiAction::Abort
    ));
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )))
    .expect("second ctrl-c");
    assert!(matches!(
        action_rx.try_recv().expect("quit action"),
        crate::UiAction::Quit
    ));
}

#[test]
fn ctrl_c_while_idle_quits_immediately() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )))
    .expect("ctrl-c");
    assert!(matches!(
        action_rx.try_recv().expect("quit action"),
        crate::UiAction::Quit
    ));
}

#[test]
fn scroll_disables_auto_follow_until_scrolled_back_to_bottom() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    for index in 0..12 {
        app.handle_agent_event(AgentEvent::MessageStart {
            message: Message::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: format!("message {index}"),
                }],
                timestamp: index,
            }),
        })
        .expect("user message");
    }

    // Chrome (spinner row + bordered input + status bar) now
    // takes ~7 rows. Give the output pane enough room for
    // the most recent message to be visible without scroll.
    let mut terminal = Terminal::new(TestBackend::new(40, 14)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw initial frame");
    let initial = render_to_string(terminal.backend());
    assert!(initial.contains("message 11"));
    assert!(!initial.contains("↑ history"));

    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::PageUp,
        KeyModifiers::NONE,
    )))
    .expect("page up");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw scrolled frame");
    let scrolled = render_to_string(terminal.backend());
    assert!(!scrolled.contains("message 11"));
    assert!(scrolled.contains("↑ history"));

    app.handle_agent_event(AgentEvent::MessageStart {
        message: Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "message 12".into(),
            }],
            timestamp: 12,
        }),
    })
    .expect("latest user message");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw with auto-scroll disabled");
    let after_new_message = render_to_string(terminal.backend());
    assert!(!after_new_message.contains("message 12"));
    assert!(after_new_message.contains("↑ history"));

    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::PageDown,
        KeyModifiers::NONE,
    )))
    .expect("page down");
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::PageDown,
        KeyModifiers::NONE,
    )))
    .expect("page down again");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw bottom frame");
    let bottom = render_to_string(terminal.backend());
    assert!(bottom.contains("message 12"));
    assert!(!bottom.contains("↑ history"));

    app.handle_agent_event(AgentEvent::MessageStart {
        message: Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "message 13".into(),
            }],
            timestamp: 13,
        }),
    })
    .expect("newest user message");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw resumed auto-follow frame");
    let resumed = render_to_string(terminal.backend());
    assert!(resumed.contains("message 13"));
    assert!(!resumed.contains("↑ history"));
}

#[test]
fn home_and_end_navigate_transcript_when_input_is_empty() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    for index in 0..16 {
        app.handle_agent_event(AgentEvent::MessageStart {
            message: Message::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: format!("message {index}"),
                }],
                timestamp: index,
            }),
        })
        .expect("user message");
    }

    let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw initial frame");
    assert!(render_to_string(terminal.backend()).contains("message 15"));

    app.handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)))
        .expect("home");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw top frame");
    let top = render_to_string(terminal.backend());
    assert!(top.contains("message 0"));
    assert!(top.contains("↑ history"));

    app.handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)))
        .expect("end");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw bottom frame");
    let bottom = render_to_string(terminal.backend());
    assert!(bottom.contains("message 15"));
    assert!(!bottom.contains("↑ history"));
}

#[test]
fn home_and_end_preserve_input_editing_when_draft_is_present() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    for index in 0..16 {
        app.handle_agent_event(AgentEvent::MessageStart {
            message: Message::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: format!("message {index}"),
                }],
                timestamp: index,
            }),
        })
        .expect("user message");
    }
    for ch in "draft".chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type draft");
    }

    let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw initial frame");

    app.handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)))
        .expect("home");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw after home");
    let after_home = render_to_string(terminal.backend());
    assert!(after_home.contains("message 15"));
    assert!(!after_home.contains("↑ history"));
    assert!(after_home.contains("> draft"));
}

#[test]
fn mouse_wheel_scrolls_transcript_history() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    for index in 0..16 {
        app.handle_agent_event(AgentEvent::MessageStart {
            message: Message::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: format!("message {index}"),
                }],
                timestamp: index,
            }),
        })
        .expect("user message");
    }

    let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw initial frame");
    assert!(render_to_string(terminal.backend()).contains("message 15"));

    app.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }))
    .expect("wheel up");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw scrolled frame");
    let scrolled = render_to_string(terminal.backend());
    assert!(!scrolled.contains("message 15"));
    assert!(scrolled.contains("↑ history"));

    app.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }))
    .expect("wheel down");
    app.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }))
    .expect("wheel down again");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw bottom frame");
    let bottom = render_to_string(terminal.backend());
    assert!(bottom.contains("message 15"));
    assert!(!bottom.contains("↑ history"));
}

#[test]
fn single_long_wrapped_assistant_message_is_navigable() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    app.load_transcript(&[Message::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text {
            text: format!("BEGIN-{}-FINAL-SUFFIX", "abcdefghij".repeat(20)),
        }],
        usage: Usage::default(),
        stop_reason: anie_protocol::StopReason::Stop,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 1,
        reasoning_details: None,
    })]);

    // 32-col terminal keeps `FINAL-SUFFIX` (12 chars) on a
    // single wrapped line regardless of where the preceding
    // prose happens to break.
    let mut terminal = Terminal::new(TestBackend::new(32, 8)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw initial frame");
    let initial = render_to_string(terminal.backend());
    assert!(initial.contains("FINAL-SUFFIX"), "{initial}");
    assert!(!initial.contains("BEGIN-"));

    app.handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)))
        .expect("home");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw top frame");
    let top = render_to_string(terminal.backend());
    assert!(top.contains("BEGIN-"));
    assert!(top.contains("↑ history"));

    app.handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)))
        .expect("end");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw bottom frame");
    let bottom = render_to_string(terminal.backend());
    assert!(bottom.contains("FINAL-SUFFIX"));
    assert!(!bottom.contains("↑ history"));
}

#[test]
fn transcript_replace_resets_scroll_state_sanely() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    for index in 0..12 {
        app.handle_agent_event(AgentEvent::MessageStart {
            message: Message::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: format!("old {index}"),
                }],
                timestamp: index,
            }),
        })
        .expect("old message");
    }

    let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw initial frame");
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::PageUp,
        KeyModifiers::NONE,
    )))
    .expect("page up");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw scrolled frame");
    assert!(render_to_string(terminal.backend()).contains("↑ history"));

    app.handle_agent_event(AgentEvent::TranscriptReplace {
        messages: vec![
            Message::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: "new 0".into(),
                }],
                timestamp: 1,
            }),
            Message::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: "new 1".into(),
                }],
                timestamp: 2,
            }),
        ],
    })
    .expect("transcript replace");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw replaced frame");
    let replaced = render_to_string(terminal.backend());
    assert!(replaced.contains("new 1"));
    assert!(!replaced.contains("old 0"));
    assert!(!replaced.contains("↑ history"));
}

#[test]
fn alt_arrow_word_movement_and_bash_title_render() {
    use crate::InputPane;

    let mut input = InputPane::new();
    for ch in "one two three".chars() {
        input.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    input.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
    input.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
    input.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
    input.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
    input.handle_key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE));
    input.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    input.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
    input.handle_key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::NONE));
    assert_eq!(input.content(), "oneZ Ytwo Xthree");

    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    app.handle_agent_event(AgentEvent::ToolExecStart {
        call_id: "call_bash".into(),
        tool_name: "bash".into(),
        args: serde_json::json!({"command": "echo hello world"}),
    })
    .expect("tool start");
    app.handle_agent_event(AgentEvent::ToolExecEnd {
        call_id: "call_bash".into(),
        result: anie_protocol::ToolResult {
            content: vec![ContentBlock::Text {
                text: "hello world".into(),
            }],
            details: serde_json::json!({}),
        },
        is_error: false,
    })
    .expect("tool end");

    let mut terminal = Terminal::new(TestBackend::new(50, 10)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());
    assert!(
        screen.contains("• Ran echo hello world"),
        "tool header missing, screen was:\n{screen}"
    );
    assert!(screen.contains("hello world"));
}

#[test]
fn replayed_tool_results_restore_titles_from_details() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    app.load_transcript(&[
        Message::ToolResult(anie_protocol::ToolResultMessage {
            tool_call_id: "call_read".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::Text {
                text: "fn main() {}".into(),
            }],
            details: serde_json::json!({"path": "src/main.rs"}),
            is_error: false,
            timestamp: 1,
        }),
        Message::ToolResult(anie_protocol::ToolResultMessage {
            tool_call_id: "call_bash".into(),
            tool_name: "bash".into(),
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
            details: serde_json::json!({"command": "echo hello", "elapsed_ms": 25}),
            is_error: false,
            timestamp: 2,
        }),
    ]);

    let mut terminal = Terminal::new(TestBackend::new(60, 16)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());
    assert!(
        screen.contains("• Read src/main.rs"),
        "screen was:\n{screen}"
    );
    assert!(screen.contains("• Ran echo hello"), "screen was:\n{screen}");
}

#[test]
fn diff_rendering_shows_added_and_removed_lines() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    app.handle_agent_event(AgentEvent::ToolExecStart {
        call_id: "call_edit".into(),
        tool_name: "edit".into(),
        args: serde_json::json!({"path": "src/main.rs"}),
    })
    .expect("tool start");
    app.handle_agent_event(AgentEvent::ToolExecEnd {
        call_id: "call_edit".into(),
        result: anie_protocol::ToolResult {
            content: vec![ContentBlock::Text {
                text: "done".into(),
            }],
            details: serde_json::json!({
                "diff": "- old line\n+ new line"
            }),
        },
        is_error: false,
    })
    .expect("tool end");

    // Bordered input (5 rows) + spinner row (1) + status (1)
    // = 7 rows of chrome. The boxed diff needs 4+ rows to
    // render its top border, two content rows, and bottom
    // border — bump the terminal to 14 rows so the full box
    // fits above the chrome.
    let mut terminal = Terminal::new(TestBackend::new(50, 14)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());
    assert!(screen.contains("┌─ edit src/main.rs"));
    assert!(screen.contains("- old line"));
    assert!(screen.contains("+ new line"));
}

#[test]
fn model_command_opens_picker_in_bottom_pane_and_keeps_transcript_visible() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(
        event_rx,
        action_tx,
        sample_models(),
        default_test_commands(),
    );
    app.status_bar_mut().provider_name = "ollama".into();
    app.status_bar_mut().model_name = "qwen3:32b".into();
    app.handle_agent_event(AgentEvent::MessageStart {
        message: Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "keep transcript visible".into(),
            }],
            timestamp: 1,
        }),
    })
    .expect("user message");

    for ch in "/model".chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type command");
    }
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("submit command");
    assert!(action_rx.try_recv().is_err());

    let mut terminal = Terminal::new(TestBackend::new(60, 14)).expect("terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());
    assert!(
        screen.contains("keep transcript visible"),
        "screen was:\n{screen}"
    );
    assert!(screen.contains("Select Model"), "screen was:\n{screen}");
    assert!(screen.contains("qwen3:32b"), "screen was:\n{screen}");
}

#[test]
fn ctrl_o_opens_picker_and_escape_restores_editor_content() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, sample_models(), Vec::new());
    app.status_bar_mut().provider_name = "ollama".into();
    app.status_bar_mut().model_name = "qwen3:32b".into();

    for ch in "draft message".chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type draft");
    }
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    )))
    .expect("open picker");
    app.handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
        .expect("cancel picker");

    let mut terminal = Terminal::new(TestBackend::new(60, 14)).expect("terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());
    assert!(screen.contains("draft message"), "screen was:\n{screen}");
    assert!(!screen.contains("Select Model"), "screen was:\n{screen}");
}

#[test]
fn picker_selection_sends_resolved_model_action() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, sample_models(), Vec::new());
    app.status_bar_mut().provider_name = "ollama".into();
    app.status_bar_mut().model_name = "qwen3:32b".into();

    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    )))
    .expect("open picker");
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("select model");

    assert!(matches!(
        action_rx.try_recv().expect("model action"),
        crate::UiAction::SetResolvedModel(model) if model.id == "qwen3:32b"
    ));
}

#[test]
fn slash_commands_route_actions_and_render_help() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(
        event_rx,
        action_tx,
        sample_models(),
        default_test_commands(),
    );

    for ch in "/model qwen3:32b".chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type model command");
    }
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("submit model command");
    assert!(matches!(
        action_rx.try_recv().expect("model action"),
        crate::UiAction::SetResolvedModel(model) if model.id == "qwen3:32b"
    ));

    for ch in "/compact".chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type compact command");
    }
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("submit compact command");
    assert!(matches!(
        action_rx.try_recv().expect("compact action"),
        crate::UiAction::Compact
    ));

    for ch in "/diff".chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type diff command");
    }
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("submit diff command");
    assert!(matches!(
        action_rx.try_recv().expect("diff action"),
        crate::UiAction::ShowDiff
    ));

    for ch in "/fork".chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type fork command");
    }
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("submit fork command");
    assert!(matches!(
        action_rx.try_recv().expect("fork action"),
        crate::UiAction::ForkSession
    ));

    for ch in "/help".chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type help command");
    }
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("submit help command");
    assert!(matches!(
        action_rx.try_recv().expect("help action"),
        crate::UiAction::ShowHelp
    ));
}

#[test]
fn new_command_sends_new_session_action() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    submit_command(&mut app, "/new");
    assert!(matches!(
        action_rx.try_recv().expect("new session action"),
        crate::UiAction::NewSession
    ));
}

#[test]
fn reload_command_sends_reload_config_action() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    submit_command(&mut app, "/reload");
    assert!(matches!(
        action_rx.try_recv().expect("reload action"),
        crate::UiAction::ReloadConfig {
            provider: None,
            model: None,
        }
    ));
}

#[test]
fn copy_command_without_messages_shows_error() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    submit_command(&mut app, "/copy");

    let mut terminal = Terminal::new(TestBackend::new(60, 12)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());
    assert!(
        screen.contains("No assistant message to copy"),
        "screen was:\n{screen}"
    );
}

#[test]
fn truncate_text_handles_unicode_without_panicking() {
    let text = "🙂".repeat(70);

    assert_eq!(crate::app::truncate_text(&text, 5), "🙂🙂🙂🙂…");
}

#[test]
fn finalize_last_assistant_preserves_provider_error_message() {
    // Regression: a provider error that carries an
    // `error_message` on the AssistantMessage must survive
    // `finalize_last_assistant` so the renderer can surface it.
    // Prior to the fix this argument was dropped, leaving the
    // user staring at a thinking block with no explanation of
    // why no text followed.
    let mut pane = OutputPane::new();
    pane.add_streaming_assistant();
    pane.finalize_last_assistant(
        String::new(),
        "planning…".into(),
        10,
        Some("model returned no visible content".into()),
    );
    let block = pane
        .blocks()
        .last()
        .expect("assistant block must exist after finalize");
    let RenderedBlock::AssistantMessage { error_message, .. } = block else {
        panic!("expected AssistantMessage, got {block:?}");
    };
    assert_eq!(
        error_message.as_deref(),
        Some("model returned no visible content")
    );
}

#[test]
fn thinking_only_assistant_block_renders_error_footer() {
    // Regression: the exact TUI scenario the user reported —
    // thinking block present, no answer text, and a provider
    // error attached. The rendered transcript must show the
    // error so the turn does not appear to end silently.
    let screen = render_assistant_block_with_error(
        "",
        "reasoning chain here",
        "model returned no visible content (only reasoning)",
        48,
        10,
    );
    assert!(
        screen.contains("• Thinking"),
        "thinking section should still render:\n{screen}"
    );
    assert!(
        screen.contains("no visible content"),
        "error footer must be visible after a thinking-only turn:\n{screen}"
    );
    // The `• Error` header is the cue that this is a provider-error
    // row, not part of the thinking content.
    assert!(
        screen.contains("• Error"),
        "error header should be flagged:\n{screen}"
    );
}

#[test]
fn assistant_block_without_error_renders_no_error_footer() {
    // Happy-path regression: healthy turns render untouched by
    // the error-footer path.
    let screen = render_assistant_block("answer text", "a plan", false, 48, 10);
    assert!(
        !screen.contains("• Error"),
        "no error header should appear on healthy turns:\n{screen}"
    );
}

#[test]
fn output_pane_last_assistant_text_skips_thinking_only_messages() {
    let mut pane = OutputPane::new();
    pane.add_block(RenderedBlock::AssistantMessage {
        text: String::new(),
        thinking: "plan only".into(),
        is_streaming: false,
        timestamp: 1,
        error_message: None,
    });
    pane.add_block(RenderedBlock::AssistantMessage {
        text: "visible answer".into(),
        thinking: "hidden reasoning".into(),
        is_streaming: false,
        timestamp: 2,
        error_message: None,
    });

    assert_eq!(pane.last_assistant_text(), Some("visible answer"));
}

#[test]
fn onboarding_slash_command_opens_overlay_locally() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    for ch in "/onboard".chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type onboard command");
    }
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("submit onboard command");

    assert!(
        action_rx.try_recv().is_err(),
        "overlay should not emit a controller action on open"
    );

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());
    assert!(
        screen.contains("Welcome to Anie — First Run"),
        "screen was:\n{screen}"
    );
}

#[test]
fn providers_slash_command_opens_provider_management_overlay() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    for ch in "/providers".chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type providers command");
    }
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("submit providers command");

    assert!(
        action_rx.try_recv().is_err(),
        "overlay should not emit a controller action on open"
    );

    let mut terminal = Terminal::new(TestBackend::new(90, 24)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());
    assert!(
        screen.contains("Configured Providers"),
        "screen was:\n{screen}"
    );
}

#[test]
fn app_transitions_back_to_idle_after_agent_end() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());
    app.handle_agent_event(AgentEvent::AgentStart)
        .expect("agent start");
    app.handle_agent_event(AgentEvent::AgentEnd { messages: vec![] })
        .expect("agent end");
    let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());
    assert!(screen.contains(":"));
    assert!(matches!(AgentUiState::Idle, AgentUiState::Idle));
}

fn render_assistant_block(
    text: &str,
    thinking: &str,
    is_streaming: bool,
    width: u16,
    height: u16,
) -> String {
    let mut pane = OutputPane::new();
    pane.add_block(RenderedBlock::AssistantMessage {
        text: text.into(),
        thinking: thinking.into(),
        is_streaming,
        timestamp: 1,
        error_message: None,
    });
    render_output_pane_to_string(&mut pane, width, height)
}

fn render_assistant_block_with_error(
    text: &str,
    thinking: &str,
    error_message: &str,
    width: u16,
    height: u16,
) -> String {
    let mut pane = OutputPane::new();
    pane.add_block(RenderedBlock::AssistantMessage {
        text: text.into(),
        thinking: thinking.into(),
        is_streaming: false,
        timestamp: 1,
        error_message: Some(error_message.into()),
    });
    render_output_pane_to_string(&mut pane, width, height)
}

fn render_output_pane_to_string(pane: &mut OutputPane, width: u16, height: u16) -> String {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    pane.render(area, &mut buffer, "⠋", false);
    render_buffer_to_string(&buffer)
}

fn render_to_string(backend: &TestBackend) -> String {
    render_buffer_to_string(backend.buffer())
}

fn render_buffer_to_string(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut rows = Vec::new();
    for y in 0..area.height {
        let mut row = String::new();
        for x in 0..area.width {
            row.push_str(buffer[(x, y)].symbol());
        }
        rows.push(row.trim_end().to_string());
    }
    rows.join("\n")
}

fn non_empty_lines(rendered: &str) -> Vec<&str> {
    rendered.lines().filter(|line| !line.is_empty()).collect()
}

fn submit_command(app: &mut App, command: &str) {
    for ch in command.chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type command");
    }
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("submit command");
}

// ---------------------------------------------------------------------------
// Thinking block display regression tests
// ---------------------------------------------------------------------------

/// Thinking text must never appear outside the `• Thinking`
/// section. Every occurrence of `thinking_text` in the
/// rendered screen must be either the header itself or a
/// `  └ ` / `    ` indented body line.
fn assert_thinking_text_only_in_gutter(screen: &str, thinking_text: &str) {
    for (line_no, line) in screen.lines().enumerate() {
        if line.contains(thinking_text) {
            let trimmed_start = line.trim_start();
            let in_body = trimmed_start.starts_with("└ ") || line.starts_with("    ");
            let is_heading = line.trim() == "• Thinking";
            assert!(
                in_body || is_heading,
                "thinking text '{}' leaked outside thinking section at line {}:\n  {}\nfull screen:\n{}",
                thinking_text,
                line_no + 1,
                line,
                screen
            );
        }
    }
}

#[test]
fn thinking_text_never_leaks_into_visible_answer_replayed() {
    let screen = render_assistant_block("final answer", "secret plan", false, 40, 10);

    assert!(screen.contains("final answer"), "screen was:\n{screen}");
    assert!(screen.contains("secret plan"), "screen was:\n{screen}");
    assert_thinking_text_only_in_gutter(&screen, "secret plan");
    // Thinking must not appear concatenated with answer text
    assert!(
        !screen.contains("secret planfinal"),
        "thinking concatenated with answer:\n{screen}"
    );
    assert!(
        !screen.contains("final answersecret"),
        "answer concatenated with thinking:\n{screen}"
    );
}

#[test]
fn thinking_text_never_leaks_into_visible_answer_streamed() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());

    // Simulate streaming: thinking first, then text, then done
    app.handle_agent_event(AgentEvent::AgentStart)
        .expect("agent start");
    app.handle_agent_event(AgentEvent::MessageStart {
        message: Message::Assistant(AssistantMessage {
            content: vec![],
            usage: Usage::default(),
            stop_reason: anie_protocol::StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock-model".into(),
            timestamp: 1,
            reasoning_details: None,
        }),
    })
    .expect("assistant start");
    app.handle_agent_event(AgentEvent::MessageDelta {
        delta: StreamDelta::ThinkingDelta("secret plan".into()),
    })
    .expect("thinking delta");
    app.handle_agent_event(AgentEvent::MessageDelta {
        delta: StreamDelta::TextDelta("final answer".into()),
    })
    .expect("text delta");
    app.handle_agent_event(AgentEvent::MessageEnd {
        message: Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "secret plan".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "final answer".into(),
                },
            ],
            usage: Usage::default(),
            stop_reason: anie_protocol::StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock-model".into(),
            timestamp: 1,
            reasoning_details: None,
        }),
    })
    .expect("message end");

    let mut terminal = Terminal::new(TestBackend::new(60, 14)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());

    assert!(screen.contains("final answer"), "screen was:\n{screen}");
    assert!(screen.contains("secret plan"), "screen was:\n{screen}");
    assert_thinking_text_only_in_gutter(&screen, "secret plan");
}

#[test]
fn multi_turn_thinking_stays_contained_in_each_message() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());

    // Load two assistant messages, each with thinking + text
    app.load_transcript(&[
        Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "first plan".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "first answer".into(),
                },
            ],
            usage: Usage::default(),
            stop_reason: anie_protocol::StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock-model".into(),
            timestamp: 1,
            reasoning_details: None,
        }),
        Message::User(anie_protocol::UserMessage {
            content: vec![ContentBlock::Text {
                text: "next question".into(),
            }],
            timestamp: 2,
        }),
        Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "second plan".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "second answer".into(),
                },
            ],
            usage: Usage::default(),
            stop_reason: anie_protocol::StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock-model".into(),
            timestamp: 3,
            reasoning_details: None,
        }),
    ]);

    let mut terminal = Terminal::new(TestBackend::new(60, 24)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());

    assert_thinking_text_only_in_gutter(&screen, "first plan");
    assert_thinking_text_only_in_gutter(&screen, "second plan");
    assert!(screen.contains("first answer"), "screen was:\n{screen}");
    assert!(screen.contains("second answer"), "screen was:\n{screen}");
}

#[test]
fn thinking_only_completed_message_shows_only_gutter() {
    // A completed (non-streaming) message with only thinking and no text
    // renders as `• Thinking` header + `  └ body` line — no leaked text.
    let screen = render_assistant_block("", "only reasoning here", false, 40, 8);

    assert_thinking_text_only_in_gutter(&screen, "only reasoning here");
    let lines = non_empty_lines(&screen);
    assert_eq!(lines.len(), 2, "unexpected lines: {lines:?}");
    assert_eq!(lines[0], "• Thinking");
    assert!(
        lines[1].trim_start().starts_with("└ "),
        "line was: {}",
        lines[1]
    );
}

#[test]
fn long_thinking_does_not_bleed_past_gutter_boundary() {
    // A long thinking block that wraps should stay entirely in the
    // bulleted body region — every wrapped line starts with `  └ ` or
    // `    ` continuation indent.
    let long_thinking = "a]b".repeat(50); // 150 chars
    let screen = render_assistant_block("done", &long_thinking, false, 30, 20);

    assert!(screen.contains("done"), "screen was:\n{screen}");
    for line in screen.lines() {
        if line.contains("a]b") {
            let trimmed = line.trim_start();
            assert!(
                trimmed.starts_with("└ ") || line.starts_with("    "),
                "wrapped thinking leaked outside section: {line}\nfull screen:\n{screen}"
            );
        }
    }
}

// =============================================================================
// Plan 11 phase C — TUI-side pre-dispatch validation.
// =============================================================================

/// Minimal catalog covering the commands exercised in Phase C
/// regression tests. Mirrors the shape produced by the CLI's
/// `builtin_commands()` without pulling in the full list.
fn phase_c_catalog() -> Vec<SlashCommandInfo> {
    const LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high"];
    const MARKDOWN_SWITCHES: &[&str] = &["on", "off"];
    const TOOL_OUTPUT_MODES: &[&str] = &["verbose", "compact"];
    const OAUTH_PROVIDERS: &[&str] = &[
        "anthropic",
        "openai-codex",
        "github-copilot",
        "google-antigravity",
        "google-gemini-cli",
    ];
    vec![
        SlashCommandInfo::builtin_with_args(
            "thinking",
            "Set reasoning effort",
            ArgumentSpec::Enumerated {
                values: LEVELS,
                required: false,
            },
            Some("[off|minimal|low|medium|high]"),
        ),
        SlashCommandInfo::builtin_with_args(
            "context-length",
            "Query or override Ollama context length",
            ArgumentSpec::ContextLengthOverride,
            Some("[N|reset]"),
        ),
        SlashCommandInfo::builtin_with_args(
            "markdown",
            "Toggle markdown rendering",
            ArgumentSpec::Enumerated {
                values: MARKDOWN_SWITCHES,
                required: false,
            },
            Some("[on|off]"),
        ),
        SlashCommandInfo::builtin_with_args(
            "tool-output",
            "Set tool-output display mode",
            ArgumentSpec::Enumerated {
                values: TOOL_OUTPUT_MODES,
                required: false,
            },
            Some("[verbose|compact]"),
        ),
        SlashCommandInfo::builtin_with_args(
            "login",
            "OAuth login instructions",
            ArgumentSpec::Enumerated {
                values: OAUTH_PROVIDERS,
                required: true,
            },
            Some("<provider>"),
        ),
        SlashCommandInfo::builtin_with_args(
            "logout",
            "Remove stored credential",
            ArgumentSpec::FreeForm { required: true },
            Some("<provider>"),
        ),
        SlashCommandInfo::builtin("compact", "Manually compact"),
        SlashCommandInfo::builtin("state", "Show persistent values"),
        SlashCommandInfo::builtin("help", "Show help"),
    ]
}

fn submit_line(app: &mut App, line: &str) {
    for ch in line.chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type char");
    }
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("submit");
}

fn last_system_message(app: &App) -> Option<String> {
    app.output_blocks()
        .iter()
        .rev()
        .find_map(|block| match block {
            RenderedBlock::SystemMessage { text } => Some(text.clone()),
            _ => None,
        })
}

#[test]
fn slash_thinking_invalid_emits_error_and_no_action() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/thinking bogus");

    let msg = last_system_message(&app).expect("expected rejection message");
    assert!(msg.contains("bogus"), "{msg}");
    assert!(msg.contains("off") && msg.contains("high"), "{msg}");
    assert!(
        action_rx.try_recv().is_err(),
        "no UiAction should be dispatched for an invalid argument"
    );
}

#[test]
fn slash_thinking_valid_dispatches_set_thinking() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/thinking high");

    let action = action_rx.try_recv().expect("valid command must dispatch");
    assert!(matches!(action, crate::UiAction::SetThinking(level) if level == "high"));
}

#[test]
fn context_length_slash_command_dispatches_ui_action() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/context-length 16384");

    let action = action_rx.try_recv().expect("valid command must dispatch");
    assert!(matches!(
        action,
        crate::UiAction::ContextLength(Some(value)) if value == "16384"
    ));
}

#[test]
fn slash_state_dispatches_show_state_action() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/state");

    let action = action_rx.try_recv().expect("/state must dispatch");
    assert!(matches!(action, crate::UiAction::ShowState));
}

#[test]
fn context_length_slash_command_query_dispatches_none() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/context-length");

    let action = action_rx.try_recv().expect("query command must dispatch");
    assert!(matches!(action, crate::UiAction::ContextLength(None)));
}

#[test]
fn context_length_slash_command_rejects_invalid_argument() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/context-length wide");

    let msg = last_system_message(&app).expect("expected rejection");
    assert!(msg.contains("wide") && msg.contains("reset"), "{msg}");
    assert!(
        action_rx.try_recv().is_err(),
        "invalid context-length argument must not dispatch"
    );
}

#[test]
fn slash_compact_with_arg_is_rejected_locally() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/compact foo");

    let msg = last_system_message(&app).expect("expected rejection");
    assert!(msg.contains("/compact"), "{msg}");
    assert!(msg.contains("no arguments"), "{msg}");
    assert!(
        action_rx.try_recv().is_err(),
        "no UiAction should be dispatched for /compact with trailing arg"
    );
}

#[test]
fn slash_markdown_no_arg_reports_current_state() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/markdown");

    let msg = last_system_message(&app).expect("expected status message");
    assert!(msg.contains("Markdown rendering is"), "{msg}");
    // Default is on.
    assert!(msg.contains("on"), "{msg}");
}

#[test]
fn slash_markdown_off_disables_rendering() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/markdown off");

    let msg = last_system_message(&app).expect("expected ack");
    assert!(msg.contains("disabled"), "{msg}");
    // /markdown is UI-only; no UiAction should reach the controller.
    assert!(
        action_rx.try_recv().is_err(),
        "no UiAction should be dispatched for /markdown"
    );
}

#[test]
fn slash_markdown_on_enables_rendering() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/markdown off");
    submit_line(&mut app, "/markdown on");

    let msg = last_system_message(&app).expect("expected ack");
    assert!(msg.contains("enabled"), "{msg}");
}

#[test]
fn slash_markdown_invalid_arg_is_rejected() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/markdown maybe");

    let msg = last_system_message(&app).expect("expected rejection");
    // The catalog's enumerated-arg validator rejects at the
    // pre-dispatch layer, so the message comes from argument
    // validation (cites the allowed values) rather than from
    // the /markdown dispatch arm.
    assert!(msg.contains("maybe"), "{msg}");
    assert!(msg.contains("on") && msg.contains("off"), "{msg}");
}

// Plan 09 PR-C — `/tool-output [verbose|compact]` tests.

#[test]
fn slash_tool_output_reports_current_state() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/tool-output");

    let msg = last_system_message(&app).expect("expected status message");
    assert!(msg.contains("Tool output mode is"), "{msg}");
    // Default is Verbose.
    assert!(msg.contains("verbose"), "{msg}");
}

#[test]
fn slash_tool_output_compact_is_ui_only() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/tool-output compact");

    let msg = last_system_message(&app).expect("expected ack");
    assert!(msg.contains("compact"), "{msg}");
    // /tool-output is UI-only; no UiAction should reach the controller.
    assert!(
        action_rx.try_recv().is_err(),
        "/tool-output must not dispatch controller work"
    );
    assert_eq!(
        app.tool_output_mode(),
        anie_config::ToolOutputMode::Compact,
        "OutputPane mode must reflect the slash-command change"
    );
}

#[test]
fn slash_tool_output_verbose_restores_default_mode() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/tool-output compact");
    submit_line(&mut app, "/tool-output verbose");

    let msg = last_system_message(&app).expect("expected ack");
    assert!(msg.contains("verbose"), "{msg}");
    assert_eq!(app.tool_output_mode(), anie_config::ToolOutputMode::Verbose,);
}

#[test]
fn slash_tool_output_invalid_arg_is_rejected() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/tool-output maybe");

    let msg = last_system_message(&app).expect("expected rejection");
    assert!(msg.contains("maybe"), "{msg}");
    // The enumerated-arg validator enumerates the allowed
    // values in its error so the user sees what's accepted.
    assert!(msg.contains("verbose") && msg.contains("compact"), "{msg}");
}

#[test]
fn slash_login_points_user_at_cli_flow() {
    // OAuth login needs a browser callback + localhost server
    // that would interfere with the alternate-screen TUI, so
    // the /login command is a documentation nudge, not an
    // actual login runner.
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/login github-copilot");

    let msg = last_system_message(&app).expect("expected instruction");
    assert!(msg.contains("anie login github-copilot"), "{msg}");
}

#[test]
fn slash_logout_without_stored_credential_reports_missing() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/logout no-such-provider");

    let msg = last_system_message(&app).expect("expected status");
    assert!(msg.contains("No stored credential"), "{msg}");
    assert!(msg.contains("no-such-provider"), "{msg}");
}

#[test]
fn slash_unknown_command_reported_without_dispatch() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), phase_c_catalog());

    submit_line(&mut app, "/does-not-exist");

    let msg = last_system_message(&app).expect("expected unknown-command message");
    assert!(msg.contains("Unknown command"), "{msg}");
    assert!(msg.contains("/does-not-exist"), "{msg}");
    assert!(
        action_rx.try_recv().is_err(),
        "no UiAction should be dispatched for an unknown command"
    );
}

// =============================================================================
// Plan 12 phase D — editor integration (popup triggers + apply).
// =============================================================================

fn type_chars(app: &mut App, s: &str) {
    for ch in s.chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )))
        .expect("type");
        // Autocomplete refreshes are debounced to fire from
        // the render loop. Tests don't drive a render cycle
        // per keystroke, so flush synchronously here to keep
        // the popup state matching what a user would see
        // after a paused typing sequence.
        app.flush_pending_autocomplete_for_test();
    }
}

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.handle_terminal_event(Event::Key(KeyEvent::new(code, modifiers)))
        .expect("press");
    app.flush_pending_autocomplete_for_test();
}

#[test]
fn typing_slash_opens_autocomplete_popup() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    type_chars(&mut app, "/");

    assert!(
        app.input_pane_is_popup_open(),
        "expected popup to open after typing '/'"
    );
}

#[test]
fn typing_filter_narrows_popup_contents() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    type_chars(&mut app, "/th");

    // Render to a frame and assert the narrowed popup shows `thinking`.
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).expect("terminal");
    terminal.draw(|frame| app.render(frame)).expect("draw");
    let screen = render_to_string(terminal.backend());
    assert!(screen.contains("thinking"), "{screen}");
    assert!(!screen.contains("providers"), "{screen}");
}

#[test]
fn enter_on_command_name_inserts_slash_and_trailing_space() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    type_chars(&mut app, "/th");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    // After applying, the input should read "/thinking " and no
    // UiAction should have been dispatched yet.
    assert_eq!(app.input_pane_contents(), "/thinking ");
    assert!(
        action_rx.try_recv().is_err(),
        "Enter on popup must not submit"
    );
}

#[test]
fn typing_slash_thinking_space_opens_enumerated_popup() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    type_chars(&mut app, "/thinking ");

    let mut terminal = Terminal::new(TestBackend::new(60, 16)).expect("terminal");
    terminal.draw(|frame| app.render(frame)).expect("draw");
    let screen = render_to_string(terminal.backend());
    assert!(screen.contains("/thinking values"), "{screen}");
    for level in ["off", "minimal", "low", "medium", "high"] {
        assert!(screen.contains(level), "level {level} missing:\n{screen}");
    }
}

#[test]
fn enter_on_enumerated_value_applies_without_trailing_space() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    // Use `me` — after adding `minimal`, just `m` is ambiguous.
    type_chars(&mut app, "/thinking me");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.input_pane_contents(), "/thinking medium");
    assert!(
        action_rx.try_recv().is_err(),
        "Enter on argument popup must not submit"
    );
}

#[test]
fn second_enter_submits_fully_typed_command() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    // Use `me` — after adding `minimal`, just `m` is ambiguous.
    type_chars(&mut app, "/thinking me");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // applies "medium"

    // After applying, the input is "/thinking medium". The popup
    // may re-open on the exact match, but the second Enter then
    // short-circuits to submit because the suggestion is a no-op.
    assert_eq!(app.input_pane_contents(), "/thinking medium");

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let action = action_rx.try_recv().expect("second Enter dispatches");
    assert!(matches!(action, crate::UiAction::SetThinking(level) if level == "medium"));
}

#[test]
fn escape_dismisses_popup_without_modifying_buffer() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    type_chars(&mut app, "/th");
    assert!(app.input_pane_is_popup_open());

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.input_pane_is_popup_open());
    assert_eq!(app.input_pane_contents(), "/th");
}

#[test]
fn arrow_keys_navigate_popup_and_skip_history_while_open() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    // Submit a history entry so Up/Down would normally recall it.
    type_chars(&mut app, "prior message");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let _ = action_rx.try_recv(); // drain the SubmitPrompt

    type_chars(&mut app, "/th");
    let before = app.input_pane_contents().to_string();
    // Up would normally load history; here it should just navigate
    // the popup (no buffer change).
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.input_pane_contents(), before);
    assert!(app.input_pane_is_popup_open());
}

#[test]
fn backspace_reopens_popup_with_updated_filter() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    type_chars(&mut app, "/thz");
    // "thz" matches nothing → popup closed.
    assert!(!app.input_pane_is_popup_open());

    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    // Back to "/th" → popup re-opens.
    assert!(app.input_pane_is_popup_open());
}

#[test]
fn popup_does_not_open_for_non_slash_input() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    type_chars(&mut app, "hello world");
    assert!(!app.input_pane_is_popup_open());
}

#[test]
fn popup_does_not_open_when_slash_is_not_at_line_start() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    type_chars(&mut app, "hello /th");
    assert!(!app.input_pane_is_popup_open());
}

#[test]
fn tab_also_applies_suggestion() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    type_chars(&mut app, "/th");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);

    assert_eq!(app.input_pane_contents(), "/thinking ");
    assert!(
        action_rx.try_recv().is_err(),
        "Tab on popup must never submit"
    );
}

// =============================================================================
// Plan 12 phase E — extensibility + toggle.
// =============================================================================

#[test]
fn disabled_autocomplete_does_not_open_popup_but_keeps_validation() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands())
        .with_autocomplete_enabled(false);

    type_chars(&mut app, "/th");
    assert!(!app.input_pane_is_popup_open());

    // Validation is still applied on submit (plan 11).
    type_chars(&mut app, "inking bogus");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        action_rx.try_recv().is_err(),
        "invalid arg must still be rejected locally"
    );
    let last = last_system_message(&app).expect("rejection");
    assert!(last.contains("bogus"), "{last}");
}

#[test]
fn extension_registered_command_appears_in_popup() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();

    let mut catalog = default_test_commands();
    catalog.push(SlashCommandInfo {
        name: "ext-foo",
        summary: "Test extension command",
        source: crate::commands::SlashCommandSource::Extension {
            extension_name: "demo".into(),
        },
        arguments: ArgumentSpec::None,
        argument_hint: None,
    });

    let mut app = App::new(event_rx, action_tx, Vec::new(), catalog);
    type_chars(&mut app, "/ext-");

    let mut terminal = Terminal::new(TestBackend::new(60, 16)).expect("terminal");
    terminal.draw(|frame| app.render(frame)).expect("draw");
    let screen = render_to_string(terminal.backend());
    assert!(screen.contains("ext-foo"), "{screen}");
}

#[test]
fn popup_description_exposes_same_argument_hint_as_help() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), default_test_commands());

    type_chars(&mut app, "/th");
    let mut terminal = Terminal::new(TestBackend::new(72, 16)).expect("terminal");
    terminal.draw(|frame| app.render(frame)).expect("draw");
    let screen = render_to_string(terminal.backend());
    assert!(
        screen.contains("[off|minimal|low|medium|high]"),
        "popup description should expose the argument hint:\n{screen}"
    );
}

#[test]
fn slash_exit_alias_still_quits_even_without_catalog_entry() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    // Empty catalog — /exit must still be honored as a /quit alias.
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());

    submit_line(&mut app, "/exit");

    let action = action_rx.try_recv().expect("exit must dispatch Quit");
    assert!(matches!(action, crate::UiAction::Quit));
    assert!(app.should_quit());
}

/// Phase 3.1: `handle_agent_event_batch` collapses
/// consecutive TextDelta events into a single append, and
/// preserves mixed-kind and non-delta event ordering.
#[test]
fn handle_agent_event_batch_coalesces_consecutive_text_deltas() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());

    app.handle_agent_event(AgentEvent::MessageStart {
        message: Message::Assistant(AssistantMessage {
            content: Vec::new(),
            usage: Usage::default(),
            stop_reason: anie_protocol::StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock-model".into(),
            timestamp: 1,
            reasoning_details: None,
        }),
    })
    .expect("assistant start");

    // Five text deltas in one burst. After batch processing
    // the streaming assistant must show the full concatenation.
    let events = vec![
        AgentEvent::MessageDelta {
            delta: StreamDelta::TextDelta("hel".into()),
        },
        AgentEvent::MessageDelta {
            delta: StreamDelta::TextDelta("lo ".into()),
        },
        AgentEvent::MessageDelta {
            delta: StreamDelta::TextDelta("wor".into()),
        },
        AgentEvent::MessageDelta {
            delta: StreamDelta::TextDelta("ld".into()),
        },
        AgentEvent::MessageDelta {
            delta: StreamDelta::TextDelta("!".into()),
        },
    ];
    app.handle_agent_event_batch(events).expect("batch");

    // The last assistant text is fully assembled.
    let text = app
        .output_blocks()
        .iter()
        .rev()
        .find_map(|b| match b {
            RenderedBlock::AssistantMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("assistant block");
    assert_eq!(text, "hello world!");
}

/// Mixed TextDelta / ThinkingDelta runs flush at kind
/// boundaries so the two streams don't cross-contaminate.
#[test]
fn handle_agent_event_batch_flushes_on_kind_change() {
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::unbounded_channel();
    let mut app = App::new(event_rx, action_tx, Vec::new(), Vec::new());

    app.handle_agent_event(AgentEvent::MessageStart {
        message: Message::Assistant(AssistantMessage {
            content: Vec::new(),
            usage: Usage::default(),
            stop_reason: anie_protocol::StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock-model".into(),
            timestamp: 1,
            reasoning_details: None,
        }),
    })
    .expect("assistant start");

    let events = vec![
        AgentEvent::MessageDelta {
            delta: StreamDelta::TextDelta("a".into()),
        },
        AgentEvent::MessageDelta {
            delta: StreamDelta::ThinkingDelta("b".into()),
        },
        AgentEvent::MessageDelta {
            delta: StreamDelta::TextDelta("c".into()),
        },
    ];
    app.handle_agent_event_batch(events).expect("batch");

    let (text, thinking) = app
        .output_blocks()
        .iter()
        .rev()
        .find_map(|b| match b {
            RenderedBlock::AssistantMessage { text, thinking, .. } => {
                Some((text.clone(), thinking.clone()))
            }
            _ => None,
        })
        .expect("assistant block");
    assert_eq!(text, "ac");
    assert_eq!(thinking, "b");
}

fn text_delta_event(text: impl Into<String>) -> AgentEvent {
    AgentEvent::MessageDelta {
        delta: StreamDelta::TextDelta(text.into()),
    }
}

fn text_delta_payload(event: &AgentEvent) -> &str {
    let AgentEvent::MessageDelta {
        delta: StreamDelta::TextDelta(text),
    } = event
    else {
        panic!("expected text delta event");
    };
    text
}

#[test]
fn agent_event_drain_limit_matches_saturated_interactive_burst() {
    let (tx, mut rx) = mpsc::channel(MAX_AGENT_EVENTS_PER_FRAME);
    for index in 0..MAX_AGENT_EVENTS_PER_FRAME {
        tx.try_send(text_delta_event(index.to_string()))
            .expect("saturated realistic burst should fit channel capacity");
    }

    let first = rx.try_recv().expect("first event");
    let batch = drain_agent_event_batch(&mut rx, first);

    assert_eq!(batch.len(), MAX_AGENT_EVENTS_PER_FRAME);
    assert!(rx.try_recv().is_err());
}

#[test]
fn bounded_agent_event_drain_preserves_order_and_leaves_remainder() {
    const EXTRA_EVENTS: usize = 8;
    let (tx, mut rx) = mpsc::channel(MAX_AGENT_EVENTS_PER_FRAME + EXTRA_EVENTS);
    for index in 0..(MAX_AGENT_EVENTS_PER_FRAME + EXTRA_EVENTS) {
        tx.try_send(text_delta_event(index.to_string()))
            .expect("test channel capacity");
    }

    let first = rx.try_recv().expect("first event");
    let first_batch = drain_agent_event_batch(&mut rx, first);

    assert_eq!(first_batch.len(), MAX_AGENT_EVENTS_PER_FRAME);
    assert_eq!(text_delta_payload(&first_batch[0]), "0");
    assert_eq!(
        text_delta_payload(first_batch.last().expect("last first-batch event")),
        (MAX_AGENT_EVENTS_PER_FRAME - 1).to_string()
    );

    let first_remaining = rx.try_recv().expect("remainder should stay queued");
    let second_batch = drain_agent_event_batch(&mut rx, first_remaining);

    assert_eq!(second_batch.len(), EXTRA_EVENTS);
    assert_eq!(
        text_delta_payload(&second_batch[0]),
        MAX_AGENT_EVENTS_PER_FRAME.to_string()
    );
    assert_eq!(
        text_delta_payload(second_batch.last().expect("last second-batch event")),
        (MAX_AGENT_EVENTS_PER_FRAME + EXTRA_EVENTS - 1).to_string()
    );
}
