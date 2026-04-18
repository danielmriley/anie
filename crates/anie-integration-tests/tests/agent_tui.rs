use std::fs;

use ratatui::{Terminal, backend::TestBackend};
use tokio::sync::mpsc;

use anie_integration_tests::helpers::*;
use anie_protocol::{AgentEvent, AssistantMessage, ContentBlock, StopReason, Usage};
use anie_provider::{ProviderError, ProviderEvent, mock::MockStreamScript};
use anie_tui::App;

fn replay_events_and_render(events: &[AgentEvent], width: u16, height: u16) -> String {
    // App::new requires channel endpoints for construction; we feed events
    // directly via handle_agent_event so neither channel is actually used.
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (action_tx, _action_rx) = mpsc::channel(8);
    let mut app = App::new(event_rx, action_tx, Vec::new());

    for event in events {
        app.handle_agent_event(event.clone()).expect("handle event");
    }

    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal.draw(|frame| app.render(frame)).expect("draw");

    let buffer = terminal.backend().buffer();
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

#[tokio::test]
async fn agent_events_render_prompt_assistant_and_tool_blocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path();
    fs::write(cwd.join("example.txt"), "hello from disk").expect("write seed");

    let provider = anie_provider::mock::MockProvider::new(vec![
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_read",
            "read",
            serde_json::json!({"path": "example.txt"}),
        )])),
        MockStreamScript::from_message(final_assistant("I read the file successfully.")),
    ]);
    let agent = build_agent(provider, real_tool_registry(cwd));
    let (_result, events) =
        run_agent_collecting_events(agent, vec![user_prompt("Read it.")], Vec::new()).await;

    let screen = replay_events_and_render(&events, 80, 30);

    assert!(
        screen.contains("Read it."),
        "user prompt missing from:\n{screen}"
    );
    assert!(
        screen.contains("I read the file successfully."),
        "final answer missing from:\n{screen}"
    );
    assert!(
        screen.contains("read example.txt"),
        "tool title missing from:\n{screen}"
    );
    assert!(
        screen.contains("hello from disk"),
        "tool result body missing from:\n{screen}"
    );
}

#[tokio::test]
async fn agent_thinking_events_render_thinking_section_above_answer() {
    let thinking_text = "Let me reason about this.";
    let answer_text = "The final answer is 42.";
    let assistant_msg = AssistantMessage {
        content: vec![
            ContentBlock::Thinking {
                thinking: thinking_text.into(),
            },
            ContentBlock::Text {
                text: answer_text.into(),
            },
        ],
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 1,
    };

    let provider = anie_provider::mock::MockProvider::new(vec![MockStreamScript::new(vec![
        Ok(ProviderEvent::Start),
        Ok(ProviderEvent::ThinkingDelta(thinking_text.into())),
        Ok(ProviderEvent::TextDelta(answer_text.into())),
        Ok(ProviderEvent::Done(assistant_msg)),
    ])]);
    let agent = build_agent(
        provider,
        std::sync::Arc::new(anie_agent::ToolRegistry::new()),
    );
    let (_result, events) =
        run_agent_collecting_events(agent, vec![user_prompt("Think.")], Vec::new()).await;

    let screen = replay_events_and_render(&events, 80, 24);

    assert!(
        screen.contains("thinking"),
        "thinking heading missing from:\n{screen}"
    );
    assert!(
        screen.contains("Let me reason about this."),
        "thinking body missing from:\n{screen}"
    );
    assert!(
        screen.contains(answer_text),
        "answer missing from:\n{screen}"
    );

    let thinking_pos = screen.find("thinking").expect("thinking heading");
    let answer_pos = screen.find(answer_text).expect("answer text");
    assert!(
        thinking_pos < answer_pos,
        "thinking should appear before answer in:\n{screen}"
    );
}

#[tokio::test]
async fn agent_stream_error_renders_error_in_tui() {
    let provider = anie_provider::mock::MockProvider::new(vec![MockStreamScript::new(vec![
        Ok(ProviderEvent::Start),
        Err(ProviderError::Stream("connection lost".into())),
    ])]);
    let agent = build_agent(
        provider,
        std::sync::Arc::new(anie_agent::ToolRegistry::new()),
    );
    let (result, events) =
        run_agent_collecting_events(agent, vec![user_prompt("Fail.")], Vec::new()).await;

    assert_eq!(
        result.terminal_error,
        Some(ProviderError::Stream("connection lost".into()))
    );

    let screen = replay_events_and_render(&events, 80, 24);
    assert!(
        screen.contains("connection lost"),
        "error missing from:\n{screen}"
    );
}

// ---------------------------------------------------------------------------
// Thinking block display regression tests (end-to-end through agent loop)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn thinking_only_provider_response_becomes_error_not_visible_thinking() {
    // A provider that returns only thinking content and then errors
    // should result in an error, not a message with leaked thinking.
    let provider = anie_provider::mock::MockProvider::new(vec![MockStreamScript::new(vec![
        Ok(ProviderEvent::Start),
        Ok(ProviderEvent::ThinkingDelta(
            "internal reasoning only".into(),
        )),
        Err(ProviderError::Stream("empty assistant response".into())),
    ])]);
    let agent = build_agent(
        provider,
        std::sync::Arc::new(anie_agent::ToolRegistry::new()),
    );
    let (result, events) =
        run_agent_collecting_events(agent, vec![user_prompt("Think.")], Vec::new()).await;

    assert!(
        result.terminal_error.is_some(),
        "expected terminal error for thinking-only response"
    );

    let screen = replay_events_and_render(&events, 80, 24);

    // The thinking text should be visible (it was streamed before the error)
    // but it must be in the gutter, not as plain text
    if screen.contains("internal reasoning only") {
        for line in screen.lines() {
            if line.contains("internal reasoning only") {
                let trimmed = line.trim();
                assert!(
                    trimmed.starts_with('\u{2502}'),
                    "thinking leaked outside gutter: {line}\nfull screen:\n{screen}"
                );
            }
        }
    }
}

#[tokio::test]
async fn multi_turn_with_thinking_keeps_thinking_in_gutter() {
    // First turn: thinking + text. Render and verify thinking stays in gutter.
    let turn1 = AssistantMessage {
        content: vec![
            ContentBlock::Thinking {
                thinking: "plan step one".into(),
            },
            ContentBlock::Text {
                text: "Here is my answer.".into(),
            },
        ],
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 1,
    };

    let provider = anie_provider::mock::MockProvider::new(vec![MockStreamScript::new(vec![
        Ok(ProviderEvent::Start),
        Ok(ProviderEvent::ThinkingDelta("plan step one".into())),
        Ok(ProviderEvent::TextDelta("Here is my answer.".into())),
        Ok(ProviderEvent::Done(turn1)),
    ])]);
    let agent = build_agent(
        provider,
        std::sync::Arc::new(anie_agent::ToolRegistry::new()),
    );
    let (_result, events) =
        run_agent_collecting_events(agent, vec![user_prompt("First.")], Vec::new()).await;

    let screen = replay_events_and_render(&events, 80, 30);

    // Thinking from turn 1 must be in gutter
    for line in screen.lines() {
        if line.contains("plan step one") {
            let trimmed = line.trim();
            assert!(
                trimmed.starts_with('\u{2502}'),
                "thinking leaked outside gutter: {line}\nfull screen:\n{screen}"
            );
        }
    }

    // Answer must be visible
    assert!(
        screen.contains("Here is my answer."),
        "answer missing:\n{screen}"
    );
}
