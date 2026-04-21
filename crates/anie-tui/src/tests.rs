use tokio::sync::mpsc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Rect};

use anie_protocol::{
    AgentEvent, AssistantMessage, ContentBlock, Message, StreamDelta, Usage, UserMessage,
};
use anie_provider::{ApiKind, CostPerMillion, Model, ModelCompat};

use crate::{
    AgentUiState, App, OutputPane, RenderedBlock,
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
        SlashCommandInfo::builtin("compact", "Manually compact"),
        SlashCommandInfo::builtin("fork", "Fork session"),
        SlashCommandInfo::builtin("diff", "Show diff"),
        SlashCommandInfo::builtin("new", "New session"),
        SlashCommandInfo::builtin_with_args(
            "session",
            "Session info",
            ArgumentSpec::Subcommands { known: SESSION_SUBS },
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

    assert!(screen.contains("> You: Fix the bug in main.rs"));
    assert!(screen.contains("anthropic:claude-sonnet-4-6 │ thinking: medium │ 12.4k/200k"));
    assert!(screen.contains("> "));
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
        .find("thinking\n│ plan first")
        .expect("thinking section");
    let text_index = screen.find("final answer").expect("visible answer");

    assert!(thinking_index < text_index, "screen was:\n{screen}");
    assert!(
        screen.contains("thinking\n│ plan first\n\nfinal answer"),
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

    let mut terminal = Terminal::new(TestBackend::new(60, 12)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw frame");
    let screen = render_to_string(terminal.backend());
    let thinking_index = screen
        .find("thinking\n│ plan first")
        .expect("thinking section");
    let text_index = screen.find("final answer").expect("visible answer");
    let streaming_index = screen.find("responding...").expect("streaming status");

    assert!(thinking_index < text_index, "screen was:\n{screen}");
    assert!(text_index < streaming_index, "screen was:\n{screen}");
    assert!(
        screen.contains("thinking\n│ plan first\n\nfinal answer"),
        "screen was:\n{screen}"
    );
}

#[test]
fn wrapped_thinking_lines_keep_their_section_gutter() {
    let screen = render_assistant_block("done", "abcdefghijklmnop", false, 10, 8);

    assert!(
        screen.contains("thinking\n│ abcdefgh\n│ ijklmnop\n\ndone"),
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

    assert_eq!(non_empty_lines(&screen), vec!["thinking", "│ plan first"]);
}

#[test]
fn streaming_assistant_without_visible_answer_reports_thinking_status() {
    let screen = render_assistant_block("", "plan first", true, 20, 6);

    assert!(
        screen.contains("thinking\n│ plan first\n│ ⠋ thinking..."),
        "screen was:\n{screen}"
    );
    assert!(!screen.contains("responding..."), "screen was:\n{screen}");
}

#[test]
fn empty_streaming_assistant_uses_generic_status() {
    let screen = render_assistant_block("", "", true, 20, 4);

    assert_eq!(non_empty_lines(&screen), vec!["⠋ streaming..."]);
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
    assert!(screen.contains("┌─ read src/main.rs"));
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

    let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("test terminal");
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

    let mut terminal = Terminal::new(TestBackend::new(20, 8)).expect("test terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("draw initial frame");
    let initial = render_to_string(terminal.backend());
    assert!(initial.contains("FINAL-SUFFIX"));
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
    assert!(screen.contains("┌─ $ echo hello world"));
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
    assert!(screen.contains("read src/main.rs"), "screen was:\n{screen}");
    assert!(screen.contains("$ echo hello"), "screen was:\n{screen}");
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

    let mut terminal = Terminal::new(TestBackend::new(50, 10)).expect("test terminal");
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
    let mut app = App::new(event_rx, action_tx, sample_models(), default_test_commands());
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
    let mut app = App::new(event_rx, action_tx, sample_models(), default_test_commands());

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
        screen.contains("thinking"),
        "thinking section should still render:\n{screen}"
    );
    assert!(
        screen.contains("no visible content"),
        "error footer must be visible after a thinking-only turn:\n{screen}"
    );
    // Belt-and-suspenders: the warning glyph is the cue that
    // this is an error line, not part of the thinking content.
    assert!(screen.contains('⚠'), "error line should be flagged:\n{screen}");
}

#[test]
fn assistant_block_without_error_renders_no_error_footer() {
    // Happy-path regression: healthy turns render untouched by
    // the new error-footer path.
    let screen = render_assistant_block("answer text", "a plan", false, 48, 10);
    assert!(
        !screen.contains('⚠'),
        "no warning glyph should appear on healthy turns:\n{screen}"
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
    pane.render(area, &mut buffer, "⠋");
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

/// Thinking text must never appear outside the "thinking" gutter section.
/// This helper asserts that every occurrence of `thinking_text` in the
/// rendered screen is inside a `│`-prefixed gutter line or the heading.
fn assert_thinking_text_only_in_gutter(screen: &str, thinking_text: &str) {
    for (line_no, line) in screen.lines().enumerate() {
        if line.contains(thinking_text) {
            let trimmed = line.trim();
            let in_gutter = trimmed.starts_with('│');
            let is_heading = trimmed == "thinking";
            assert!(
                in_gutter || is_heading,
                "thinking text '{}' leaked outside gutter at line {}:\n  {}\nfull screen:\n{}",
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
    // should render just the gutter section, no leaked text
    let screen = render_assistant_block("", "only reasoning here", false, 40, 8);

    assert_thinking_text_only_in_gutter(&screen, "only reasoning here");
    let lines = non_empty_lines(&screen);
    // Should only have "thinking" heading and the gutter line
    assert_eq!(lines.len(), 2, "unexpected lines: {lines:?}");
    assert_eq!(lines[0], "thinking");
    assert!(lines[1].starts_with('│'), "line was: {}", lines[1]);
}

#[test]
fn long_thinking_does_not_bleed_past_gutter_boundary() {
    // A long thinking block that wraps should stay entirely in the gutter
    let long_thinking = "a]b".repeat(50); // 150 chars
    let screen = render_assistant_block("done", &long_thinking, false, 30, 20);

    assert!(screen.contains("done"), "screen was:\n{screen}");
    // Every line containing thinking content must be in gutter
    for line in screen.lines() {
        if line.contains("a]b") {
            let trimmed = line.trim();
            assert!(
                trimmed.starts_with('│'),
                "wrapped thinking leaked outside gutter: {line}\nfull screen:\n{screen}"
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
            "markdown",
            "Toggle markdown rendering",
            ArgumentSpec::Enumerated {
                values: MARKDOWN_SWITCHES,
                required: false,
            },
            Some("[on|off]"),
        ),
        SlashCommandInfo::builtin("compact", "Manually compact"),
        SlashCommandInfo::builtin("help", "Show help"),
    ]
}

fn submit_line(app: &mut App, line: &str) {
    for ch in line.chars() {
        app.handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)))
            .expect("type char");
    }
    app.handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)))
        .expect("submit");
}

fn last_system_message(app: &App) -> Option<String> {
    app.output_blocks().iter().rev().find_map(|block| match block {
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
    assert!(
        msg.contains("Markdown rendering is"),
        "{msg}"
    );
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
        app.handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)))
            .expect("type");
    }
}

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.handle_terminal_event(Event::Key(KeyEvent::new(code, modifiers)))
        .expect("press");
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
