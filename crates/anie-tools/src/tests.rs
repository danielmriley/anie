use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_agent::{AgentLoop, AgentLoopConfig, Tool, ToolError, ToolExecutionMode, ToolRegistry};
use anie_protocol::{
    AssistantMessage, ContentBlock, Message, StopReason, ToolCall, Usage, UserMessage,
};
use anie_provider::{
    ApiKind, CostPerMillion, Model, ModelCompat, ProviderError, ProviderRegistry,
    RequestOptionsResolver, ResolvedRequestOptions, ThinkingLevel,
    mock::{MockProvider, MockStreamScript},
};

use crate::edit::{
    MAX_EDIT_ARGUMENT_BYTES, MAX_EDIT_COUNT, MAX_EDIT_INPUT_FILE_BYTES, MAX_EDIT_NEW_TEXT_BYTES,
    MAX_EDIT_OLD_TEXT_BYTES, MAX_EDIT_OUTPUT_FILE_BYTES,
};
use crate::{BashPolicy, BashTool, EditTool, FileMutationQueue, ReadTool, WriteTool};

struct StaticResolver;

#[async_trait]
impl RequestOptionsResolver for StaticResolver {
    async fn resolve(
        &self,
        _model: &Model,
        _context: &[Message],
    ) -> Result<ResolvedRequestOptions, ProviderError> {
        Ok(ResolvedRequestOptions::default())
    }
}

fn sample_model() -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock Model".into(),
        provider: "mock".into(),
        api: ApiKind::OpenAICompletions,
        base_url: "http://localhost".into(),
        context_window: 128_000,
        max_tokens: 8_192,
        supports_reasoning: false,
        reasoning_capabilities: None,
        supports_images: false,
        cost_per_million: CostPerMillion::zero(),
        replay_capabilities: None,
        compat: ModelCompat::None,
    }
}

fn user_prompt(text: &str) -> Message {
    Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        timestamp: 1,
    })
}

fn assistant_with_tool_call(
    id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        })],
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 1,
        reasoning_details: None,
    }
}

fn final_assistant(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 2,
        reasoning_details: None,
    }
}

fn text_content(result: &anie_protocol::ToolResult) -> String {
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

#[tokio::test]
async fn read_tool_reads_small_text_file() {
    let tempdir = tempdir().expect("tempdir");
    let path = tempdir.path().join("hello.txt");
    tokio::fs::write(&path, "hello\nworld\n")
        .await
        .expect("write file");

    let tool = ReadTool::new(tempdir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "path": "hello.txt" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("read succeeds");

    assert_eq!(text_content(&result), "hello\nworld");
}

#[tokio::test]
async fn read_tool_supports_offset_and_limit() {
    let tempdir = tempdir().expect("tempdir");
    let path = tempdir.path().join("numbers.txt");
    tokio::fs::write(&path, "one\ntwo\nthree\nfour\n")
        .await
        .expect("write file");

    let tool = ReadTool::new(tempdir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "path": "numbers.txt", "offset": 2, "limit": 2 }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("read succeeds");

    assert_eq!(text_content(&result), "two\nthree");
}

#[tokio::test]
async fn read_tool_truncates_at_line_limit() {
    let tempdir = tempdir().expect("tempdir");
    let path = tempdir.path().join("many_lines.txt");
    let contents = (0..2_100)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    tokio::fs::write(&path, contents).await.expect("write file");

    let tool = ReadTool::new(tempdir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "path": "many_lines.txt" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("read succeeds");

    let text = text_content(&result);
    // PR 5.2 of `docs/code_review_2026-04-27/`: the footer no
    // longer carries an exact remaining-line count, since
    // streaming reads stop as soon as the cap is hit and
    // computing a precise count would re-scan the rest of
    // the file.
    assert!(
        text.contains("[output truncated. Use offset to read more.]"),
        "got: {text}"
    );
    // 2000 lines == MAX_READ_LINES were shown.
    assert_eq!(text.lines().count() - 1, 2000); // -1 for the footer line
}

#[tokio::test]
async fn read_tool_truncates_at_byte_limit() {
    let tempdir = tempdir().expect("tempdir");
    let path = tempdir.path().join("wide.txt");
    let contents = "x".repeat(60 * 1024);
    tokio::fs::write(&path, contents).await.expect("write file");

    let tool = ReadTool::new(tempdir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "path": "wide.txt" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("read succeeds");

    let text = text_content(&result);
    // Footer wording updated for PR 5.2 — see comment above.
    assert!(
        text.contains("[output truncated. Use offset to read more.]"),
        "got: {text}"
    );
}

#[tokio::test]
async fn read_tool_detects_and_encodes_images() {
    let tempdir = tempdir().expect("tempdir");
    let path = tempdir.path().join("image.png");
    let png_bytes = vec![137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 0];
    tokio::fs::write(&path, &png_bytes)
        .await
        .expect("write image");

    let tool = ReadTool::new(tempdir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "path": "image.png" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("image read succeeds");

    assert!(matches!(
        result.content.first(),
        Some(ContentBlock::Image { media_type, .. }) if media_type == "image/png"
    ));
}

/// PR 5.2 of `docs/code_review_2026-04-27/`. Behavioral
/// proxy for the streaming-read invariant: a huge file with a
/// small `limit` returns bounded output without scanning the
/// rest of the file. We can't directly assert "did not load
/// the full body" without instrumenting the reader, but a
/// 64 MiB sparse file finished within seconds (< 5s in CI)
/// is a strong indication — the pre-streaming implementation
/// allocated the full body and would either OOM or take much
/// longer.
#[tokio::test]
async fn read_tool_does_not_load_entire_large_text_file_for_small_limit() {
    let tempdir = tempdir().expect("tempdir");
    let path = tempdir.path().join("huge.log");
    // 64 MiB: large enough to dwarf the 50 KiB / 2000 line
    // caps, small enough that creating it is fast on tmpfs.
    // We write actual bytes (not sparse) so the streaming
    // reader has real content to walk; sparse files would
    // confuse line-counting heuristics on some filesystems.
    let mut content = String::with_capacity(64 * 1024 * 1024);
    for i in 0..(64 * 1024 / 8) {
        // Each iteration writes ~8 bytes (a 5-digit index +
        // newline). 8192 iterations → ~64 KiB; do 8192 *
        // 1024 = 64 MiB total.
        for j in 0..1024 {
            content.push_str(&format!("{i:05}-{j:03}\n"));
        }
    }
    tokio::fs::write(&path, &content)
        .await
        .expect("write huge file");

    let tool = ReadTool::new(tempdir.path());
    let started = std::time::Instant::now();
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "path": "huge.log", "limit": 20 }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("read succeeds");
    let elapsed = started.elapsed();

    let text = text_content(&result);
    let line_count = text.lines().count();
    assert_eq!(
        line_count, 20,
        "limit=20 must return exactly 20 lines, got {line_count}",
    );
    // 5 seconds is generous; the streaming reader should
    // finish in < 100ms on tmpfs. The pre-streaming
    // implementation walked all 8M lines to compute
    // `total_lines`, which dominated the runtime.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "streaming read took {elapsed:?}; regression suggests full-file scan",
    );
}

/// PR 5.2 of `docs/code_review_2026-04-27/`. A pathological
/// newline-less file (or one with a single very long line)
/// must NOT grow the per-line buffer to the file size. The
/// `read_one_line` helper caps the buffer at
/// `MAX_LINE_BUFFER_BYTES` (4× `MAX_READ_BYTES`); after that
/// the read returns `LineEnd::Cap` and the streaming loop
/// stops with `truncated = true`. Build a 1 MiB file of
/// non-newline bytes and confirm the read completes quickly
/// with bounded output — a regression that used unbounded
/// `read_until` would still work for 1 MiB but would fail
/// on a 1 GiB single-line file. We cap the test at 1 MiB to
/// keep tmpfs usage modest while still being well above
/// `MAX_LINE_BUFFER_BYTES = 200 KiB`.
#[tokio::test]
async fn read_tool_caps_line_buffer_for_newline_less_file() {
    let tempdir = tempdir().expect("tempdir");
    let path = tempdir.path().join("oneline.txt");
    let content = "x".repeat(1024 * 1024);
    tokio::fs::write(&path, &content).await.expect("write file");

    let tool = ReadTool::new(tempdir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "path": "oneline.txt" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("read succeeds");

    let text = text_content(&result);
    // The "line" gets trimmed at MAX_READ_BYTES = 50 KiB
    // before display; with the truncation footer added the
    // surfaced text should sit comfortably under 60 KiB —
    // not anywhere near the source's 1 MiB.
    assert!(
        text.len() < 60 * 1024,
        "surfaced text {} bytes; expected < 60 KiB. Regression suggests \
         the line buffer ballooned to file size.",
        text.len(),
    );
    assert!(
        text.contains("[output truncated. Use offset to read more.]"),
        "got: {text}"
    );
}

/// PR 5.1 of `docs/code_review_2026-04-27/`. The image cap
/// must be enforced from `metadata.len()` BEFORE the file
/// body lands in memory. Use `set_len` to grow a sparse file
/// to 11 MiB without writing 11 MiB of bytes to disk —
/// `metadata.len()` reports the logical size, so the pre-read
/// check rejects, while a regression that called
/// `tokio::fs::read` first would allocate 11 MiB before the
/// cap fired.
#[tokio::test]
async fn read_tool_rejects_oversized_image_via_metadata() {
    let tempdir = tempdir().expect("tempdir");
    let path = tempdir.path().join("huge.png");
    let file = std::fs::File::create(&path).expect("create image");
    // 11 MiB is just over MAX_IMAGE_BYTES (10 MiB).
    file.set_len(11 * 1024 * 1024).expect("set_len");
    drop(file);

    let tool = ReadTool::new(tempdir.path());
    let error = tool
        .execute(
            "call",
            serde_json::json!({ "path": "huge.png" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("oversized image should reject");
    match error {
        anie_agent::ToolError::ExecutionFailed(msg) => {
            assert!(msg.contains("too large"), "got: {msg}");
            assert!(msg.contains("huge.png"), "got: {msg}");
        }
        other => panic!("expected ExecutionFailed, got: {other:?}"),
    }
}

#[tokio::test]
async fn read_tool_returns_error_for_missing_file() {
    let tempdir = tempdir().expect("tempdir");
    let tool = ReadTool::new(tempdir.path());
    let error = tool
        .execute(
            "call",
            serde_json::json!({ "path": "missing.txt" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("missing file should error");

    assert!(
        matches!(error, anie_agent::ToolError::ExecutionFailed(message) if message.contains("missing.txt"))
    );
}

#[tokio::test]
async fn write_tool_creates_new_file() {
    let tempdir = tempdir().expect("tempdir");
    let tool = WriteTool::new(tempdir.path());
    tool.execute(
        "call",
        serde_json::json!({ "path": "new.txt", "content": "hello" }),
        CancellationToken::new(),
        None,
    )
    .await
    .expect("write succeeds");

    let written = tokio::fs::read_to_string(tempdir.path().join("new.txt"))
        .await
        .expect("read written file");
    assert_eq!(written, "hello");
}

#[tokio::test]
async fn write_tool_overwrites_existing_file() {
    let tempdir = tempdir().expect("tempdir");
    let path = tempdir.path().join("existing.txt");
    tokio::fs::write(&path, "old").await.expect("seed file");

    let tool = WriteTool::new(tempdir.path());
    tool.execute(
        "call",
        serde_json::json!({ "path": "existing.txt", "content": "new" }),
        CancellationToken::new(),
        None,
    )
    .await
    .expect("write succeeds");

    let written = tokio::fs::read_to_string(path)
        .await
        .expect("read written file");
    assert_eq!(written, "new");
}

#[tokio::test]
async fn write_tool_creates_parent_directories() {
    let tempdir = tempdir().expect("tempdir");
    let tool = WriteTool::new(tempdir.path());
    tool.execute(
        "call",
        serde_json::json!({ "path": "nested/dir/file.txt", "content": "hello" }),
        CancellationToken::new(),
        None,
    )
    .await
    .expect("write succeeds");

    let written = tokio::fs::read_to_string(tempdir.path().join("nested/dir/file.txt"))
        .await
        .expect("read written file");
    assert_eq!(written, "hello");
}

#[tokio::test]
async fn write_tool_honors_cancellation_before_write() {
    let tempdir = tempdir().expect("tempdir");
    let tool = WriteTool::new(tempdir.path());
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = tool
        .execute(
            "call",
            serde_json::json!({ "path": "cancelled.txt", "content": "hello" }),
            cancel,
            None,
        )
        .await
        .expect_err("cancelled write should fail");
    assert_eq!(error, anie_agent::ToolError::Aborted);
}

#[tokio::test]
async fn file_mutation_queue_canonicalizes_alias_paths() {
    let tempdir = tempdir().expect("tempdir");
    let file_path = tempdir.path().join("file.txt");
    tokio::fs::write(&file_path, "seed")
        .await
        .expect("seed file");

    let queue = Arc::new(FileMutationQueue::new());
    let alias_path = tempdir.path().join("./file.txt");
    let queue_clone = Arc::clone(&queue);
    let file_path_clone = file_path.clone();

    let start = Instant::now();
    let first = tokio::spawn(async move {
        queue_clone
            .with_lock(&file_path_clone, || async {
                tokio::time::sleep(Duration::from_millis(150)).await;
            })
            .await;
    });

    tokio::time::sleep(Duration::from_millis(25)).await;
    queue
        .with_lock(&alias_path, || async {
            assert!(start.elapsed() >= Duration::from_millis(150));
        })
        .await;

    first.await.expect("first task");
}

#[cfg(unix)]
#[tokio::test]
async fn bash_tool_runs_simple_command() {
    let tempdir = tempdir().expect("tempdir");
    let tool = BashTool::new(tempdir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "command": "echo hello" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("command succeeds");

    assert!(text_content(&result).contains("hello"));
}

#[tokio::test]
async fn bash_policy_blocks_denied_command_before_spawn() {
    let tempdir = tempdir().expect("tempdir");
    let tool = BashTool::with_policy(
        tempdir.path(),
        BashPolicy {
            enabled: true,
            deny_commands: vec!["touch".into()],
            deny_patterns: Vec::new(),
        },
    );

    let error = tool
        .execute(
            "call",
            serde_json::json!({ "command": "touch blocked.txt" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("policy should block");

    assert!(
        matches!(error, ToolError::ExecutionFailed(message) if message.contains("command 'touch' is denied"))
    );
    assert!(!tempdir.path().join("blocked.txt").exists());
}

#[tokio::test]
async fn bash_policy_blocks_denied_command_basename() {
    let tempdir = tempdir().expect("tempdir");
    let tool = BashTool::with_policy(
        tempdir.path(),
        BashPolicy {
            enabled: true,
            deny_commands: vec!["touch".into()],
            deny_patterns: Vec::new(),
        },
    );

    let error = tool
        .execute(
            "call",
            serde_json::json!({ "command": "/usr/bin/touch blocked.txt" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("policy should block");

    assert!(
        matches!(error, ToolError::ExecutionFailed(message) if message.contains("command 'touch' is denied"))
    );
    assert!(!tempdir.path().join("blocked.txt").exists());
}

#[tokio::test]
async fn bash_policy_blocks_denied_regex_pattern() {
    let tempdir = tempdir().expect("tempdir");
    let tool = BashTool::with_policy(
        tempdir.path(),
        BashPolicy {
            enabled: true,
            deny_commands: Vec::new(),
            deny_patterns: vec![r"git\s+push\s+--force".into()],
        },
    );

    let error = tool
        .execute(
            "call",
            serde_json::json!({ "command": "git push --force origin main" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("policy should block");

    assert!(
        matches!(error, ToolError::ExecutionFailed(message) if message.contains("matched deny pattern"))
    );
}

#[tokio::test]
async fn bash_policy_disabled_does_not_block() {
    let tempdir = tempdir().expect("tempdir");
    let tool = BashTool::with_policy(
        tempdir.path(),
        BashPolicy {
            enabled: false,
            deny_commands: vec!["echo".into()],
            deny_patterns: vec!["echo".into()],
        },
    );

    let result = tool
        .execute(
            "call",
            serde_json::json!({ "command": "echo allowed" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("disabled policy should not block");

    assert!(text_content(&result).contains("allowed"));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_tool_captures_multiline_output() {
    let tempdir = tempdir().expect("tempdir");
    let tool = BashTool::new(tempdir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "command": "printf 'a\\nb\\n'" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("command succeeds");

    assert_eq!(text_content(&result), "a\nb");
}

#[cfg(unix)]
#[tokio::test]
async fn bash_tool_propagates_exit_code_failures() {
    let tempdir = tempdir().expect("tempdir");
    let tool = BashTool::new(tempdir.path());
    let error = tool
        .execute(
            "call",
            serde_json::json!({ "command": "echo fail && exit 7" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("command should fail");

    assert!(
        matches!(error, anie_agent::ToolError::ExecutionFailed(message) if message.contains("status 7") && message.contains("fail"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_tool_enforces_timeout() {
    let tempdir = tempdir().expect("tempdir");
    let tool = BashTool::new(tempdir.path());
    let error = tool
        .execute(
            "call",
            serde_json::json!({ "command": "sleep 2", "timeout": 1 }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("command should time out");

    assert_eq!(error, anie_agent::ToolError::Timeout(1));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_tool_truncates_large_output() {
    let tempdir = tempdir().expect("tempdir");
    let tool = BashTool::new(tempdir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "command": "seq 1 3000" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("command succeeds");

    assert!(text_content(&result).contains("[output truncated]"));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_tool_captures_stderr() {
    let tempdir = tempdir().expect("tempdir");
    let tool = BashTool::new(tempdir.path());
    let error = tool
        .execute(
            "call",
            serde_json::json!({ "command": "echo err >&2 && exit 3" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("command should fail");

    assert!(
        matches!(error, anie_agent::ToolError::ExecutionFailed(message) if message.contains("err"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_tool_honors_cancellation() {
    let tempdir = tempdir().expect("tempdir");
    let tool = BashTool::new(tempdir.path());
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    let handle = tokio::spawn(async move {
        tool.execute(
            "call",
            serde_json::json!({ "command": "sleep 10" }),
            cancel_clone,
            None,
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();

    let result = handle.await.expect("join task");
    assert_eq!(
        result.expect_err("command should abort"),
        anie_agent::ToolError::Aborted
    );
}

#[tokio::test]
async fn agent_loop_and_tools_support_end_to_end_read_write_flow() {
    let tempdir = tempdir().expect("tempdir");
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(WriteTool::new(tempdir.path())));
    tools.register(Arc::new(ReadTool::new(tempdir.path())));

    let mut providers = ProviderRegistry::new();
    providers.register(
        ApiKind::OpenAICompletions,
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_call(
                "call_write",
                "write",
                serde_json::json!({ "path": "hello.txt", "content": "hi there" }),
            )),
            MockStreamScript::from_message(assistant_with_tool_call(
                "call_read",
                "read",
                serde_json::json!({ "path": "hello.txt" }),
            )),
            MockStreamScript::from_message(final_assistant("done")),
        ])),
    );

    let agent = AgentLoop::new(
        Arc::new(providers),
        Arc::new(tools),
        AgentLoopConfig::new(
            sample_model(),
            "You are a test agent".into(),
            ThinkingLevel::Off,
            ToolExecutionMode::Sequential,
            Arc::new(StaticResolver),
        ),
    );

    let (event_tx, _event_rx) = mpsc::channel(64);
    let result = agent
        .run(
            vec![user_prompt("write then read")],
            Vec::new(),
            event_tx,
            CancellationToken::new(),
        )
        .await;

    let tool_results = result
        .generated_messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(tool_result) => Some(tool_result),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(tool_results.len(), 2);
    assert_eq!(
        tokio::fs::read_to_string(tempdir.path().join("hello.txt"))
            .await
            .expect("written file"),
        "hi there"
    );
    assert!(
        tool_results[1]
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text.contains("hi there")))
    );
}

#[tokio::test]
async fn edit_tool_applies_exact_replacements_and_returns_diff() {
    let tempdir = tempdir().expect("tempdir");
    tokio::fs::write(
        tempdir.path().join("main.rs"),
        "fn main() {\n    println!(\"old\");\n}\n",
    )
    .await
    .expect("seed file");

    let tool = EditTool::new(tempdir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({
                "path": "main.rs",
                "edits": [{
                    "oldText": "println!(\"old\");",
                    "newText": "println!(\"new\");",
                }]
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("edit succeeds");

    let written = tokio::fs::read_to_string(tempdir.path().join("main.rs"))
        .await
        .expect("read file");
    assert!(written.contains("println!(\"new\");"));
    let diff = result
        .details
        .get("diff")
        .and_then(serde_json::Value::as_str)
        .expect("diff text");
    assert!(diff.contains("-    println!(\"old\");"));
    assert!(diff.contains("+    println!(\"new\");"));
}

#[tokio::test]
async fn edit_tool_detects_duplicate_matches() {
    let tempdir = tempdir().expect("tempdir");
    tokio::fs::write(tempdir.path().join("dup.txt"), "same\nsame\n")
        .await
        .expect("seed file");

    let tool = EditTool::new(tempdir.path());
    let error = tool
        .execute(
            "call",
            serde_json::json!({
                "path": "dup.txt",
                "edits": [{ "oldText": "same", "newText": "different" }]
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("duplicate match should fail");

    assert!(
        matches!(error, anie_agent::ToolError::ExecutionFailed(message) if message.contains("matched 2 regions"))
    );
}

#[tokio::test]
async fn edit_tool_detects_overlapping_replacements() {
    let tempdir = tempdir().expect("tempdir");
    tokio::fs::write(tempdir.path().join("overlap.txt"), "abcdef")
        .await
        .expect("seed file");

    let tool = EditTool::new(tempdir.path());
    let error = tool
        .execute(
            "call",
            serde_json::json!({
                "path": "overlap.txt",
                "edits": [
                    { "oldText": "abc", "newText": "ABC" },
                    { "oldText": "bcd", "newText": "BCD" }
                ]
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("overlap should fail");

    assert!(
        matches!(error, anie_agent::ToolError::ExecutionFailed(message) if message.contains("overlaps edit"))
    );
}

#[tokio::test]
async fn edit_tool_rejects_too_many_edits_before_reading_file() {
    let tempdir = tempdir().expect("tempdir");
    let tool = EditTool::new(tempdir.path());
    let edits = (0..=MAX_EDIT_COUNT)
        .map(|index| {
            serde_json::json!({
                "oldText": format!("old-{index}"),
                "newText": "new",
            })
        })
        .collect::<Vec<_>>();

    let error = tool
        .execute(
            "call",
            serde_json::json!({
                "path": "missing.txt",
                "edits": edits,
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("too many edits should fail before reading");

    assert!(
        matches!(error, anie_agent::ToolError::ExecutionFailed(ref message)
            if message.contains("at most 100") && message.contains("Split this")),
        "{error:?}"
    );
}

#[tokio::test]
async fn edit_tool_rejects_oversized_old_text_before_matching() {
    let tempdir = tempdir().expect("tempdir");
    let tool = EditTool::new(tempdir.path());

    let error = tool
        .execute(
            "call",
            serde_json::json!({
                "path": "missing.txt",
                "edits": [{
                    "oldText": "x".repeat(MAX_EDIT_OLD_TEXT_BYTES + 1),
                    "newText": "replacement",
                }],
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("oversized oldText should fail before matching");

    assert!(
        matches!(error, anie_agent::ToolError::ExecutionFailed(ref message)
            if message.contains("oldText") && message.contains(&MAX_EDIT_OLD_TEXT_BYTES.to_string())),
        "{error:?}"
    );
}

#[tokio::test]
async fn edit_tool_rejects_oversized_new_text_before_matching() {
    let tempdir = tempdir().expect("tempdir");
    let tool = EditTool::new(tempdir.path());

    let error = tool
        .execute(
            "call",
            serde_json::json!({
                "path": "missing.txt",
                "edits": [{
                    "oldText": "target",
                    "newText": "x".repeat(MAX_EDIT_NEW_TEXT_BYTES + 1),
                }],
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("oversized newText should fail before matching");

    assert!(
        matches!(error, anie_agent::ToolError::ExecutionFailed(ref message)
            if message.contains("newText") && message.contains(&MAX_EDIT_NEW_TEXT_BYTES.to_string())),
        "{error:?}"
    );
}

#[tokio::test]
async fn edit_tool_rejects_combined_argument_budget_before_matching() {
    let tempdir = tempdir().expect("tempdir");
    let tool = EditTool::new(tempdir.path());
    let chunk = "x".repeat(MAX_EDIT_ARGUMENT_BYTES / 4);
    let edits = (0..5)
        .map(|index| {
            serde_json::json!({
                "oldText": format!("target-{index}"),
                "newText": chunk,
            })
        })
        .collect::<Vec<_>>();

    let error = tool
        .execute(
            "call",
            serde_json::json!({
                "path": "missing.txt",
                "edits": edits,
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("combined edit budget should fail before matching");

    assert!(
        matches!(error, anie_agent::ToolError::ExecutionFailed(ref message)
            if message.contains("edit arguments") && message.contains(&MAX_EDIT_ARGUMENT_BYTES.to_string())),
        "{error:?}"
    );
}

#[tokio::test]
async fn edit_tool_rejects_oversized_input_file_before_matching() {
    let tempdir = tempdir().expect("tempdir");
    let path = tempdir.path().join("large.txt");
    tokio::fs::write(&path, vec![b'a'; MAX_EDIT_INPUT_FILE_BYTES + 1])
        .await
        .expect("seed oversized input");
    let tool = EditTool::new(tempdir.path());

    let error = tool
        .execute(
            "call",
            serde_json::json!({
                "path": "large.txt",
                "edits": [{
                    "oldText": "a",
                    "newText": "b",
                }],
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("oversized input file should fail");

    assert!(
        matches!(error, anie_agent::ToolError::ExecutionFailed(ref message)
            if message.contains("edit input files") && message.contains(&MAX_EDIT_INPUT_FILE_BYTES.to_string())),
        "{error:?}"
    );
}

#[tokio::test]
async fn edit_tool_rejects_oversized_output_and_preserves_original_file() {
    let tempdir = tempdir().expect("tempdir");
    let path = tempdir.path().join("expand.txt");
    let prefix = "A0\nA1\nA2\n";
    let filler = "z".repeat(MAX_EDIT_INPUT_FILE_BYTES - prefix.len());
    let original = format!("{prefix}{filler}");
    tokio::fs::write(&path, &original).await.expect("seed file");
    let tool = EditTool::new(tempdir.path());
    let expansion_budget = MAX_EDIT_OUTPUT_FILE_BYTES - MAX_EDIT_INPUT_FILE_BYTES;
    let replacement = "x".repeat((expansion_budget / 3) + 4);

    let error = tool
        .execute(
            "call",
            serde_json::json!({
                "path": "expand.txt",
                "edits": [
                    { "oldText": "A0", "newText": replacement.clone() },
                    { "oldText": "A1", "newText": replacement.clone() },
                    { "oldText": "A2", "newText": replacement },
                ],
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect_err("oversized output should fail");

    assert!(
        matches!(error, anie_agent::ToolError::ExecutionFailed(ref message)
            if message.contains("edit outputs") && message.contains(&MAX_EDIT_OUTPUT_FILE_BYTES.to_string())),
        "{error:?}"
    );
    assert_eq!(
        tokio::fs::read_to_string(&path)
            .await
            .expect("read original"),
        original,
        "failed output-size check must not modify the file"
    );
}

#[tokio::test]
async fn edit_tool_preserves_bom_and_crlf() {
    let tempdir = tempdir().expect("tempdir");
    let bytes = [0xEF, 0xBB, 0xBF]
        .into_iter()
        .chain("line1\r\nline2\r\n".as_bytes().iter().copied())
        .collect::<Vec<_>>();
    tokio::fs::write(tempdir.path().join("bom.txt"), bytes)
        .await
        .expect("seed file");

    let tool = EditTool::new(tempdir.path());
    tool.execute(
        "call",
        serde_json::json!({
            "path": "bom.txt",
            "edits": [{ "oldText": "line2", "newText": "updated" }]
        }),
        CancellationToken::new(),
        None,
    )
    .await
    .expect("edit succeeds");

    let written = tokio::fs::read(tempdir.path().join("bom.txt"))
        .await
        .expect("read file");
    assert!(written.starts_with(&[0xEF, 0xBB, 0xBF]));
    let text = String::from_utf8(written[3..].to_vec()).expect("utf8");
    assert!(text.contains("updated\r\n"));
    assert!(!text.contains("updated\nline"));
}

#[tokio::test]
async fn edit_tool_can_fuzzily_match_whitespace_runs() {
    let tempdir = tempdir().expect("tempdir");
    tokio::fs::write(
        tempdir.path().join("fuzzy.txt"),
        "fn  main() {\n    ok();\n}\n",
    )
    .await
    .expect("seed file");

    let tool = EditTool::new(tempdir.path());
    tool.execute(
        "call",
        serde_json::json!({
            "path": "fuzzy.txt",
            "edits": [{ "oldText": "fn main() {", "newText": "fn main() { // updated" }]
        }),
        CancellationToken::new(),
        None,
    )
    .await
    .expect("fuzzy edit succeeds");

    let written = tokio::fs::read_to_string(tempdir.path().join("fuzzy.txt"))
        .await
        .expect("read file");
    assert!(written.contains("fn main() { // updated"));
}
