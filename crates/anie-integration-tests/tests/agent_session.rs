use std::fs;

use anie_integration_tests::helpers::*;
use anie_protocol::{ContentBlock, Message};
use anie_provider::mock::MockStreamScript;

#[tokio::test]
async fn prompt_and_assistant_response_persist_to_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path();

    let provider = anie_provider::mock::MockProvider::new(vec![MockStreamScript::from_message(
        final_assistant("The answer is 42."),
    )]);
    let agent = build_agent(provider, real_tool_registry(cwd));
    let prompt = user_prompt("What is the answer?");
    let (result, _events) =
        run_agent_collecting_events(agent, vec![prompt.clone()], Vec::new()).await;

    assert_eq!(result.generated_messages.len(), 1);
    assert!(matches!(
        &result.generated_messages[0],
        Message::Assistant(_)
    ));

    let context = persist_and_reopen(cwd, &prompt, &result);
    assert_eq!(context.messages.len(), 2);
    assert!(matches!(&context.messages[0].message, Message::User(_)));
    assert!(matches!(
        &context.messages[1].message,
        Message::Assistant(_)
    ));
}

#[tokio::test]
async fn read_tool_call_persists_file_contents_in_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path();
    fs::create_dir_all(cwd.join("src")).expect("mkdir");
    fs::write(
        cwd.join("src/main.rs"),
        "fn main() { println!(\"hello\"); }",
    )
    .expect("write seed");

    let provider = anie_provider::mock::MockProvider::new(vec![
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_read",
            "read",
            serde_json::json!({"path": "src/main.rs"}),
        )])),
        MockStreamScript::from_message(final_assistant("I've read the file.")),
    ]);
    let agent = build_agent(provider, real_tool_registry(cwd));
    let prompt = user_prompt("Read the file.");
    let (result, _events) =
        run_agent_collecting_events(agent, vec![prompt.clone()], Vec::new()).await;

    assert_eq!(result.generated_messages.len(), 3);
    let tool_result = result
        .generated_messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(tr) => Some(tr),
            _ => None,
        })
        .expect("tool result");
    let content_text = tool_result
        .content
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .expect("text content");
    assert!(
        content_text.contains("fn main()"),
        "tool result was: {content_text}"
    );
    assert_eq!(
        tool_result.details.get("path").and_then(|v| v.as_str()),
        Some("src/main.rs")
    );

    let context = persist_and_reopen(cwd, &prompt, &result);
    assert_eq!(context.messages.len(), 4);
}

#[tokio::test]
async fn edit_tool_modifies_file_and_persists_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path();
    fs::write(cwd.join("hello.txt"), "Hello, world!").expect("write seed");

    let provider = anie_provider::mock::MockProvider::new(vec![
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_edit",
            "edit",
            serde_json::json!({
                "path": "hello.txt",
                "edits": [{"oldText": "world", "newText": "anie"}]
            }),
        )])),
        MockStreamScript::from_message(final_assistant("Edit complete.")),
    ]);
    let agent = build_agent(provider, real_tool_registry(cwd));
    let prompt = user_prompt("Edit the file.");
    let (result, _events) =
        run_agent_collecting_events(agent, vec![prompt.clone()], Vec::new()).await;

    let on_disk = fs::read_to_string(cwd.join("hello.txt")).expect("read edited file");
    assert_eq!(on_disk, "Hello, anie!");

    let tool_result = result
        .generated_messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(tr) => Some(tr),
            _ => None,
        })
        .expect("tool result");
    assert!(!tool_result.is_error);
    let diff = tool_result
        .details
        .get("diff")
        .and_then(|v| v.as_str())
        .expect("diff in details");
    assert!(diff.contains('-') || diff.contains('+'), "diff was: {diff}");

    let ctx = persist_and_reopen(cwd, &prompt, &result);
    let reopened_tool = ctx
        .messages
        .iter()
        .find_map(|m| match &m.message {
            Message::ToolResult(tr) => Some(tr),
            _ => None,
        })
        .expect("tool result in context");
    assert!(reopened_tool.details.get("diff").is_some());
}

#[tokio::test]
async fn bash_tool_captures_output_and_elapsed_time() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path();

    let provider = anie_provider::mock::MockProvider::new(vec![
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_bash",
            "bash",
            serde_json::json!({"command": "echo integration-test-output"}),
        )])),
        MockStreamScript::from_message(final_assistant("Command complete.")),
    ]);
    let agent = build_agent(provider, real_tool_registry(cwd));
    let prompt = user_prompt("Run the command.");
    let (result, _events) =
        run_agent_collecting_events(agent, vec![prompt.clone()], Vec::new()).await;

    let tool_result = result
        .generated_messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(tr) => Some(tr),
            _ => None,
        })
        .expect("tool result");
    let content_text = tool_result
        .content
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .expect("text content");
    assert!(
        content_text.contains("integration-test-output"),
        "content was: {content_text}"
    );
    assert!(tool_result.details.get("elapsed_ms").is_some());
    assert_eq!(
        tool_result.details.get("command").and_then(|v| v.as_str()),
        Some("echo integration-test-output")
    );

    let ctx = persist_and_reopen(cwd, &prompt, &result);
    let reopened_tool = ctx
        .messages
        .iter()
        .find_map(|m| match &m.message {
            Message::ToolResult(tr) => Some(tr),
            _ => None,
        })
        .expect("tool result in context");
    assert!(reopened_tool.details.get("elapsed_ms").is_some());
}

#[tokio::test]
async fn multi_turn_tool_loop_produces_correct_message_sequence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path();
    fs::write(cwd.join("data.txt"), "alpha").expect("write seed");

    let provider = anie_provider::mock::MockProvider::new(vec![
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_read",
            "read",
            serde_json::json!({"path": "data.txt"}),
        )])),
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_bash",
            "bash",
            serde_json::json!({"command": "echo done"}),
        )])),
        MockStreamScript::from_message(final_assistant("All tasks complete.")),
    ]);
    let agent = build_agent(provider, real_tool_registry(cwd));
    let prompt = user_prompt("Do both tasks.");
    let (result, _events) =
        run_agent_collecting_events(agent, vec![prompt.clone()], Vec::new()).await;

    assert_eq!(result.generated_messages.len(), 5);
    let types: Vec<&str> = result
        .generated_messages
        .iter()
        .map(|m| match m {
            Message::Assistant(_) => "Assistant",
            Message::ToolResult(_) => "ToolResult",
            Message::User(_) => "User",
            Message::Custom(_) => "Custom",
        })
        .collect();
    assert_eq!(
        types,
        vec![
            "Assistant",
            "ToolResult",
            "Assistant",
            "ToolResult",
            "Assistant"
        ]
    );

    let context = persist_and_reopen(cwd, &prompt, &result);
    assert_eq!(context.messages.len(), 6);
    let ctx_types: Vec<&str> = context
        .messages
        .iter()
        .map(|m| match &m.message {
            Message::User(_) => "User",
            Message::Assistant(_) => "Assistant",
            Message::ToolResult(_) => "ToolResult",
            Message::Custom(_) => "Custom",
        })
        .collect();
    assert_eq!(
        ctx_types,
        vec![
            "User",
            "Assistant",
            "ToolResult",
            "Assistant",
            "ToolResult",
            "Assistant"
        ]
    );
}

// ── Error / edge-case tests ──────────────────────────────────────────

#[tokio::test]
async fn read_tool_with_nonexistent_path_returns_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path();

    let provider = anie_provider::mock::MockProvider::new(vec![
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_read",
            "read",
            serde_json::json!({"path": "does_not_exist.txt"}),
        )])),
        MockStreamScript::from_message(final_assistant("The file was missing.")),
    ]);
    let agent = build_agent(provider, real_tool_registry(cwd));
    let prompt = user_prompt("Read a missing file.");
    let (result, _events) =
        run_agent_collecting_events(agent, vec![prompt.clone()], Vec::new()).await;

    let tool_result = result
        .generated_messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(tr) => Some(tr),
            _ => None,
        })
        .expect("tool result");
    assert!(tool_result.is_error, "expected is_error for missing file");
    let content_text = tool_result
        .content
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .expect("error text");
    assert!(
        content_text.contains("does_not_exist.txt"),
        "error should mention the path: {content_text}"
    );

    // Agent should still complete (the provider gets another turn).
    assert!(result.terminal_error.is_none());
    assert!(
        result.generated_messages.len() >= 2,
        "expected at least tool-result + final answer"
    );
}

#[tokio::test]
async fn edit_tool_with_unmatched_old_text_returns_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path();
    fs::write(cwd.join("data.txt"), "Hello, world!").expect("write seed");

    let provider = anie_provider::mock::MockProvider::new(vec![
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_edit",
            "edit",
            serde_json::json!({
                "path": "data.txt",
                "edits": [{"oldText": "this text is not in the file", "newText": "replacement"}]
            }),
        )])),
        MockStreamScript::from_message(final_assistant("Edit failed, understood.")),
    ]);
    let agent = build_agent(provider, real_tool_registry(cwd));
    let prompt = user_prompt("Edit the file.");
    let (result, _events) =
        run_agent_collecting_events(agent, vec![prompt.clone()], Vec::new()).await;

    let tool_result = result
        .generated_messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(tr) => Some(tr),
            _ => None,
        })
        .expect("tool result");
    assert!(
        tool_result.is_error,
        "expected is_error for unmatched oldText"
    );

    // File should remain unchanged.
    let on_disk = fs::read_to_string(cwd.join("data.txt")).expect("read file");
    assert_eq!(on_disk, "Hello, world!");

    assert!(result.terminal_error.is_none());
}

#[tokio::test]
async fn bash_tool_with_nonzero_exit_returns_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path();

    let provider = anie_provider::mock::MockProvider::new(vec![
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_bash",
            "bash",
            serde_json::json!({"command": "exit 42"}),
        )])),
        MockStreamScript::from_message(final_assistant("Command failed.")),
    ]);
    let agent = build_agent(provider, real_tool_registry(cwd));
    let prompt = user_prompt("Run a failing command.");
    let (result, _events) =
        run_agent_collecting_events(agent, vec![prompt.clone()], Vec::new()).await;

    let tool_result = result
        .generated_messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(tr) => Some(tr),
            _ => None,
        })
        .expect("tool result");
    assert!(
        tool_result.is_error,
        "expected is_error for non-zero exit code"
    );
    let content_text = tool_result
        .content
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .expect("error text");
    assert!(
        content_text.contains("42"),
        "error should mention exit code: {content_text}"
    );

    assert!(result.terminal_error.is_none());
}
