use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_agent::{
    AgentLoop, AgentLoopConfig, Tool, ToolError, ToolExecutionContext, ToolExecutionMode,
    ToolRegistry,
};
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
use crate::{
    ApplyPatchTool, BashPolicy, BashTool, EditTool, FileMutationQueue, ReadTool, WriteTool,
};

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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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

/// Plan 05 PR C: small-context model gets a 1 KB byte
/// budget for read tool output. A 10 KB file (well over
/// the floor and under `MAX_READ_LINES`) must come back
/// truncated to ~1 KB plus the truncation footer, proving
/// the per-call budget shrunk with the context window.
#[tokio::test]
async fn read_tool_truncates_to_effective_budget_for_small_window() {
    let tempdir = tempdir().expect("tempdir");
    let path = tempdir.path().join("wide.txt");
    // 10 KB on a single line so the line-cap path doesn't
    // mask the byte-budget path.
    let contents = "x".repeat(10 * 1024);
    tokio::fs::write(&path, contents).await.expect("write file");

    let tool = ReadTool::new(tempdir.path());
    let small_ctx = ToolExecutionContext {
        context_window: 8_192,
    };
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "path": "wide.txt" }),
            CancellationToken::new(),
            None,
            &small_ctx,
        )
        .await
        .expect("read succeeds");

    let text = text_content(&result);
    assert!(
        text.contains("[output truncated. Use offset to read more.]"),
        "small-window read should surface truncation; got: {text}",
    );
    // The body before the footer must be ~1 KB (the floor),
    // not 10 KB. A length above 2 KB means the budget
    // didn't shrink with the window.
    assert!(
        text.len() <= 2_048,
        "small-window read body should fit ~1KB budget; got {} bytes",
        text.len(),
    );
}

/// Plan 05 PR C: regression guard. A 200K-window model
/// gets a 20 KB effective budget (10 % of 200K, capped by
/// the 50 KB `MAX_READ_BYTES`). A 10 KB file fits without
/// truncation — proves the cloud path is not collapsing
/// to the small-window floor.
#[tokio::test]
async fn read_tool_keeps_full_output_for_cloud_window() {
    let tempdir = tempdir().expect("tempdir");
    let path = tempdir.path().join("medium.txt");
    let contents = "x".repeat(10 * 1024);
    tokio::fs::write(&path, contents).await.expect("write file");

    let tool = ReadTool::new(tempdir.path());
    let cloud_ctx = ToolExecutionContext {
        context_window: 200_000,
    };
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "path": "medium.txt" }),
            CancellationToken::new(),
            None,
            &cloud_ctx,
        )
        .await
        .expect("read succeeds");

    let text = text_content(&result);
    assert!(
        !text.contains("[output truncated. Use offset to read more.]"),
        "cloud-window read should fit without truncation; got: body of {} bytes",
        text.len(),
    );
    assert!(
        text.len() >= 10 * 1024,
        "cloud-window read should return the whole 10 KB file; got {} bytes",
        text.len(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
        &ToolExecutionContext::default(),
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
        &ToolExecutionContext::default(),
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
        &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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

#[tokio::test]
async fn with_locks_acquires_distinct_paths_and_runs_operation() {
    let tempdir = tempdir().expect("tempdir");
    let a = tempdir.path().join("a.txt");
    let b = tempdir.path().join("b.txt");
    let queue = FileMutationQueue::new();
    let ran = queue.with_locks(&[a, b], || async { 42 }).await;
    assert_eq!(ran, 42);
}

#[tokio::test]
async fn with_locks_serializes_two_callers_contending_on_a_shared_path() {
    let tempdir = tempdir().expect("tempdir");
    let shared = tempdir.path().join("shared.txt");
    let other = tempdir.path().join("other.txt");
    let queue = Arc::new(FileMutationQueue::new());

    let start = Instant::now();
    let queue_clone = Arc::clone(&queue);
    let shared_clone = shared.clone();
    let first = tokio::spawn(async move {
        queue_clone
            .with_locks(&[shared_clone], || async {
                tokio::time::sleep(Duration::from_millis(150)).await;
            })
            .await;
    });

    tokio::time::sleep(Duration::from_millis(25)).await;
    // Second caller locks {shared, other}; it must wait on the shared key.
    queue
        .with_locks(&[shared, other], || async {
            assert!(start.elapsed() >= Duration::from_millis(150));
        })
        .await;
    first.await.expect("first task");
}

#[tokio::test]
async fn with_locks_is_deadlock_free_under_reversed_path_order() {
    let tempdir = tempdir().expect("tempdir");
    let a = tempdir.path().join("a.txt");
    let b = tempdir.path().join("b.txt");
    let queue = Arc::new(FileMutationQueue::new());

    // Two callers take the same two locks in opposite request order.
    // Because with_locks sorts keys, neither can hold one while waiting
    // on the other — both complete (no deadlock) within the timeout.
    let q1 = Arc::clone(&queue);
    let (a1, b1) = (a.clone(), b.clone());
    let t1 = tokio::spawn(async move {
        for _ in 0..20 {
            q1.with_locks(&[a1.clone(), b1.clone()], || async {}).await;
        }
    });
    let q2 = Arc::clone(&queue);
    let t2 = tokio::spawn(async move {
        for _ in 0..20 {
            q2.with_locks(&[b.clone(), a.clone()], || async {}).await;
        }
    });

    let joined = tokio::time::timeout(Duration::from_secs(5), async {
        t1.await.expect("t1");
        t2.await.expect("t2");
    })
    .await;
    assert!(joined.is_ok(), "with_locks deadlocked under reversed order");
}

#[tokio::test]
async fn with_locks_dedupes_repeated_path_so_it_does_not_self_deadlock() {
    let tempdir = tempdir().expect("tempdir");
    let a = tempdir.path().join("a.txt");
    let queue = FileMutationQueue::new();
    // The same path twice must dedupe to one lock, not deadlock on itself.
    let done = tokio::time::timeout(
        Duration::from_secs(2),
        queue.with_locks(&[a.clone(), a], || async { true }),
    )
    .await;
    assert_eq!(done.ok(), Some(true));
}

#[tokio::test]
async fn with_lock_still_serializes_single_path_after_refactor() {
    // Regression: with_lock now delegates to with_locks; single-path
    // serialization must still hold.
    let tempdir = tempdir().expect("tempdir");
    let file = tempdir.path().join("f.txt");
    let queue = Arc::new(FileMutationQueue::new());
    let start = Instant::now();
    let q = Arc::clone(&queue);
    let f = file.clone();
    let first = tokio::spawn(async move {
        q.with_lock(&f, || async {
            tokio::time::sleep(Duration::from_millis(120)).await;
        })
        .await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    queue
        .with_lock(&file, || async {
            assert!(start.elapsed() >= Duration::from_millis(120));
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
        )
        .await
        .expect("command succeeds");

    assert!(text_content(&result).contains("[output truncated]"));
}

/// Plan 05 PR B: with an 8K-context model the byte budget
/// floors at `MIN_TOOL_OUTPUT_BUDGET_BYTES` (1 KB), so
/// bash output that would have fit on cloud models has to
/// compress to ≤ ~1 KB plus a truncation marker.
///
/// `seq 1 1500` produces ~6.4 KB of stdout (1500 lines,
/// under the `MAX_READ_LINES = 2000` cap so the byte
/// budget is the only thing that can trip truncation) —
/// well over the 1 KB floor — so we expect the truncation
/// marker and a body bounded near the floor.
#[cfg(unix)]
#[tokio::test]
async fn bash_tool_truncates_stdout_to_effective_budget_for_small_window() {
    let tempdir = tempdir().expect("tempdir");
    let tool = BashTool::new(tempdir.path());
    let small_ctx = ToolExecutionContext {
        context_window: 8_192,
    };
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "command": "seq 1 1500" }),
            CancellationToken::new(),
            None,
            &small_ctx,
        )
        .await
        .expect("command succeeds");

    let body = text_content(&result);
    assert!(
        body.contains("[output truncated]"),
        "small-context output must surface truncation; got {body:?}",
    );
    // Floor is 1024 bytes. The collector renders at most
    // `byte_budget` bytes plus the truncation marker, so
    // anything above ~2 KB is a regression — the budget
    // didn't shrink with the window.
    assert!(
        body.len() <= 2_500,
        "small-context output should fit ~1KB budget; got {} bytes",
        body.len(),
    );
}

/// Plan 05 PR B: regression guard against shrinking cloud
/// behavior. A 200K-window model still uses an effective
/// 20 KB byte budget (10 % of 200K, capped against the
/// 50 KB `MAX_READ_BYTES` constant). `seq 1 1500` is
/// ~6.4 KB and 1500 lines — well under both the byte
/// budget and the 2000-line cap — so the cloud path must
/// return the full output without a truncation marker.
#[cfg(unix)]
#[tokio::test]
async fn bash_tool_keeps_larger_budget_for_cloud_window() {
    let tempdir = tempdir().expect("tempdir");
    let tool = BashTool::new(tempdir.path());
    let cloud_ctx = ToolExecutionContext {
        context_window: 200_000,
    };
    let result = tool
        .execute(
            "call",
            serde_json::json!({ "command": "seq 1 1500" }),
            CancellationToken::new(),
            None,
            &cloud_ctx,
        )
        .await
        .expect("command succeeds");

    let body = text_content(&result);
    assert!(
        !body.contains("[output truncated]"),
        "cloud-context output should fit without truncation; got marker in body of {} bytes",
        body.len(),
    );
    assert!(
        body.len() > 5_000,
        "cloud-context body should be much larger than the 1KB floor; got {} bytes",
        body.len(),
    );
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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

/// Golden: after the matching engine was extracted into `text_match`
/// (apply_patch/PR2), `edit`'s observable output — success message and
/// rendered diff — must be byte-identical to before the refactor.
#[tokio::test]
async fn edit_tool_output_unchanged_after_engine_extraction() {
    let tempdir = tempdir().expect("tempdir");
    tokio::fs::write(tempdir.path().join("g.txt"), "alpha\nbeta\ngamma\n")
        .await
        .expect("seed");
    let tool = EditTool::new(tempdir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({
                "path": "g.txt",
                "edits": [{ "oldText": "beta", "newText": "BETA" }]
            }),
            CancellationToken::new(),
            None,
            &ToolExecutionContext::default(),
        )
        .await
        .expect("edit succeeds");

    assert_eq!(text_content(&result), "Applied 1 edit to g.txt");
    let diff = result.details["diff"].as_str().expect("diff");
    assert_eq!(diff, " alpha\n-beta\n+BETA\n gamma");
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
            &ToolExecutionContext::default(),
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
        &ToolExecutionContext::default(),
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
        &ToolExecutionContext::default(),
    )
    .await
    .expect("fuzzy edit succeeds");

    let written = tokio::fs::read_to_string(tempdir.path().join("fuzzy.txt"))
        .await
        .expect("read file");
    assert!(written.contains("fn main() { // updated"));
}

// ===================== apply_patch (PR4) =====================

async fn run_apply_patch(
    tool: &ApplyPatchTool,
    patch: &str,
) -> Result<anie_protocol::ToolResult, ToolError> {
    tool.execute(
        "call",
        serde_json::json!({ "patch": patch }),
        CancellationToken::new(),
        None,
        &ToolExecutionContext::default(),
    )
    .await
}

#[tokio::test]
async fn apply_patch_updates_single_file_with_two_hunks() {
    let dir = tempdir().expect("tempdir");
    tokio::fs::write(dir.path().join("a.rs"), "one\ntwo\nthree\nfour\n")
        .await
        .expect("seed");
    let tool = ApplyPatchTool::new(dir.path());
    let patch = "*** Begin Patch\n*** Update File: a.rs\n one\n-two\n+TWO\n@@\n three\n-four\n+FOUR\n*** End Patch";
    run_apply_patch(&tool, patch).await.expect("apply");
    let got = tokio::fs::read_to_string(dir.path().join("a.rs"))
        .await
        .expect("read");
    assert_eq!(got, "one\nTWO\nthree\nFOUR\n");
}

#[tokio::test]
async fn apply_patch_creates_file_from_add_section() {
    let dir = tempdir().expect("tempdir");
    let tool = ApplyPatchTool::new(dir.path());
    let patch = "*** Begin Patch\n*** Add File: sub/new.rs\n+fn main() {}\n*** End Patch";
    run_apply_patch(&tool, patch).await.expect("apply");
    let got = tokio::fs::read_to_string(dir.path().join("sub/new.rs"))
        .await
        .expect("read");
    assert_eq!(got, "fn main() {}\n");
}

#[tokio::test]
async fn apply_patch_deletes_existing_file() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("dead.rs");
    tokio::fs::write(&path, "junk\n").await.expect("seed");
    let tool = ApplyPatchTool::new(dir.path());
    run_apply_patch(
        &tool,
        "*** Begin Patch\n*** Delete File: dead.rs\n*** End Patch",
    )
    .await
    .expect("apply");
    assert!(!path.exists());
}

#[tokio::test]
async fn apply_patch_applies_changes_across_three_files_in_one_call() {
    let dir = tempdir().expect("tempdir");
    tokio::fs::write(dir.path().join("up.rs"), "alpha\n")
        .await
        .expect("seed up");
    tokio::fs::write(dir.path().join("del.rs"), "bye\n")
        .await
        .expect("seed del");
    let tool = ApplyPatchTool::new(dir.path());
    let patch = "*** Begin Patch\n*** Update File: up.rs\n-alpha\n+ALPHA\n*** Add File: add.rs\n+new\n*** Delete File: del.rs\n*** End Patch";
    run_apply_patch(&tool, patch).await.expect("apply");
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("up.rs"))
            .await
            .unwrap(),
        "ALPHA\n"
    );
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("add.rs"))
            .await
            .unwrap(),
        "new\n"
    );
    assert!(!dir.path().join("del.rs").exists());
}

#[tokio::test]
async fn apply_patch_writes_nothing_when_any_hunk_fails_to_match() {
    // File A's hunk is valid; file B's hunk is stale. The whole call must
    // abort and leave BOTH files untouched on disk.
    let dir = tempdir().expect("tempdir");
    tokio::fs::write(dir.path().join("a.rs"), "good\n")
        .await
        .expect("seed a");
    tokio::fs::write(dir.path().join("b.rs"), "real\n")
        .await
        .expect("seed b");
    let tool = ApplyPatchTool::new(dir.path());
    let patch = "*** Begin Patch\n*** Update File: a.rs\n-good\n+GOOD\n*** Update File: b.rs\n-not-present\n+nope\n*** End Patch";
    let err = run_apply_patch(&tool, patch).await.expect_err("must fail");
    assert!(matches!(err, ToolError::ExecutionFailed(_)), "{err:?}");
    // Neither file changed.
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("a.rs"))
            .await
            .unwrap(),
        "good\n"
    );
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("b.rs"))
            .await
            .unwrap(),
        "real\n"
    );
}

#[tokio::test]
async fn apply_patch_rejects_add_over_existing_file() {
    let dir = tempdir().expect("tempdir");
    tokio::fs::write(dir.path().join("exists.rs"), "x\n")
        .await
        .expect("seed");
    let tool = ApplyPatchTool::new(dir.path());
    let err = run_apply_patch(
        &tool,
        "*** Begin Patch\n*** Add File: exists.rs\n+y\n*** End Patch",
    )
    .await
    .expect_err("must fail");
    assert!(matches!(err, ToolError::ExecutionFailed(m) if m.contains("already exists")));
}

#[tokio::test]
async fn apply_patch_rejects_delete_of_missing_file() {
    let dir = tempdir().expect("tempdir");
    let tool = ApplyPatchTool::new(dir.path());
    let err = run_apply_patch(
        &tool,
        "*** Begin Patch\n*** Delete File: ghost.rs\n*** End Patch",
    )
    .await
    .expect_err("must fail");
    assert!(matches!(err, ToolError::ExecutionFailed(m) if m.contains("does not exist")));
}

#[tokio::test]
async fn apply_patch_aborts_when_cancelled_before_write() {
    let dir = tempdir().expect("tempdir");
    tokio::fs::write(dir.path().join("a.rs"), "x\n")
        .await
        .expect("seed");
    let tool = ApplyPatchTool::new(dir.path());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = tool
        .execute(
            "call",
            serde_json::json!({ "patch": "*** Begin Patch\n*** Update File: a.rs\n-x\n+y\n*** End Patch" }),
            cancel,
            None,
            &ToolExecutionContext::default(),
        )
        .await
        .expect_err("cancelled");
    assert!(matches!(err, ToolError::Aborted));
    // The cancel landed before the write phase: the file is untouched.
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("a.rs"))
            .await
            .unwrap(),
        "x\n"
    );
}

#[tokio::test]
async fn apply_patch_serializes_against_edit_on_the_same_shared_queue() {
    // Holding the shared queue's lock for a path must block an
    // apply_patch on that path until released — proving apply_patch
    // goes through the same FileMutationQueue as edit/write.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("a.rs");
    tokio::fs::write(&path, "x\n").await.expect("seed");
    let queue = Arc::new(FileMutationQueue::new());
    let tool = ApplyPatchTool::with_queue(dir.path(), Arc::clone(&queue));

    let started = Arc::new(tokio::sync::Notify::new());
    let started_clone = Arc::clone(&started);
    let hold_queue = Arc::clone(&queue);
    let hold_path = path.clone();
    let holder = tokio::spawn(async move {
        hold_queue
            .with_lock(&hold_path, || async move {
                started_clone.notify_one();
                tokio::time::sleep(Duration::from_millis(200)).await;
            })
            .await;
    });

    started.notified().await;
    let t0 = Instant::now();
    run_apply_patch(
        &tool,
        "*** Begin Patch\n*** Update File: a.rs\n-x\n+y\n*** End Patch",
    )
    .await
    .expect("apply");
    assert!(
        t0.elapsed() >= Duration::from_millis(150),
        "apply_patch should have waited on the held lock"
    );
    holder.await.expect("holder");
    assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "y\n");
}

// ===================== apply_patch / edit transparency (PR5) =====================

#[tokio::test]
async fn apply_patch_dry_run_returns_diff_without_touching_disk() {
    let dir = tempdir().expect("tempdir");
    tokio::fs::write(dir.path().join("a.rs"), "x\n")
        .await
        .expect("seed");
    let tool = ApplyPatchTool::new(dir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: a.rs\n-x\n+y\n*** End Patch",
                "dry_run": true
            }),
            CancellationToken::new(),
            None,
            &ToolExecutionContext::default(),
        )
        .await
        .expect("dry run");
    assert_eq!(result.details["applied"], serde_json::json!(false));
    let diff = result.details["files"][0]["diff"].as_str().expect("diff");
    assert!(diff.contains("-x") && diff.contains("+y"), "{diff}");
    // Disk is untouched.
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("a.rs"))
            .await
            .unwrap(),
        "x\n"
    );
}

#[tokio::test]
async fn apply_patch_reports_fuzzy_hunk_count_in_details() {
    let dir = tempdir().expect("tempdir");
    // File has extra internal spacing; the hunk's context differs only in
    // whitespace, forcing the fuzzy fallback.
    tokio::fs::write(dir.path().join("a.rs"), "let  x   =  1;\n")
        .await
        .expect("seed");
    let tool = ApplyPatchTool::new(dir.path());
    let result = run_apply_patch(
        &tool,
        "*** Begin Patch\n*** Update File: a.rs\n-let x = 1;\n+let x = 2;\n*** End Patch",
    )
    .await
    .expect("apply");
    assert_eq!(
        result.details["files"][0]["fuzzy_hunks"],
        serde_json::json!(1)
    );
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("a.rs"))
            .await
            .unwrap(),
        "let x = 2;\n"
    );
}

#[tokio::test]
async fn edit_tool_message_notes_when_a_match_was_whitespace_fuzzy() {
    let dir = tempdir().expect("tempdir");
    tokio::fs::write(dir.path().join("a.rs"), "let  x   =  1;\n")
        .await
        .expect("seed");
    let tool = EditTool::new(dir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({
                "path": "a.rs",
                "edits": [{ "oldText": "let x = 1;", "newText": "let x = 2;" }]
            }),
            CancellationToken::new(),
            None,
            &ToolExecutionContext::default(),
        )
        .await
        .expect("edit");
    assert!(
        text_content(&result).contains("matched ignoring whitespace"),
        "{}",
        text_content(&result)
    );
    assert_eq!(result.details["fuzzy_edits"], serde_json::json!(1));
}

#[tokio::test]
async fn edit_tool_message_omits_fuzzy_note_when_all_matches_exact() {
    let dir = tempdir().expect("tempdir");
    tokio::fs::write(dir.path().join("a.rs"), "let x = 1;\n")
        .await
        .expect("seed");
    let tool = EditTool::new(dir.path());
    let result = tool
        .execute(
            "call",
            serde_json::json!({
                "path": "a.rs",
                "edits": [{ "oldText": "let x = 1;", "newText": "let x = 2;" }]
            }),
            CancellationToken::new(),
            None,
            &ToolExecutionContext::default(),
        )
        .await
        .expect("edit");
    assert!(!text_content(&result).contains("ignoring whitespace"));
    assert_eq!(result.details["fuzzy_edits"], serde_json::json!(0));
}

#[tokio::test]
async fn edit_tool_definition_documents_whitespace_fallback() {
    let dir = tempdir().expect("tempdir");
    let tool = EditTool::new(dir.path());
    let def = tool.definition();
    assert!(
        def.description.to_lowercase().contains("whitespace"),
        "edit description must document the whitespace fallback"
    );
}

#[tokio::test]
async fn apply_patch_preserves_crlf_and_bom_on_update() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("crlf.rs");
    // BOM + CRLF line endings.
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"one\r\ntwo\r\n");
    tokio::fs::write(&path, &bytes).await.expect("seed");
    let tool = ApplyPatchTool::new(dir.path());
    run_apply_patch(
        &tool,
        "*** Begin Patch\n*** Update File: crlf.rs\n one\n-two\n+TWO\n*** End Patch",
    )
    .await
    .expect("apply");
    let got = tokio::fs::read(&path).await.expect("read");
    assert_eq!(&got[..3], &[0xEF, 0xBB, 0xBF], "BOM preserved");
    let text = String::from_utf8(got[3..].to_vec()).expect("utf8");
    assert_eq!(text, "one\r\nTWO\r\n", "CRLF preserved");
}

// ===================== bash sandbox (sandbox/PR5) =====================

async fn run_bash(tool: &BashTool, command: &str) -> Result<anie_protocol::ToolResult, ToolError> {
    tool.execute(
        "call",
        serde_json::json!({ "command": command }),
        CancellationToken::new(),
        None,
        &ToolExecutionContext::default(),
    )
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn bash_with_sandbox_disabled_behaves_identically_to_today() {
    let dir = tempdir().expect("tempdir");
    // No sandbox spec => today's behavior; details record sandboxed=false.
    let tool = BashTool::with_sandbox(dir.path(), BashPolicy::default(), None);
    let result = run_bash(&tool, "echo hello").await.expect("runs");
    assert!(text_content(&result).contains("hello"));
    assert_eq!(result.details["sandboxed"], serde_json::json!(false));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_sandbox_setup_failure_surfaces_typed_sandbox_setup_error() {
    let dir = tempdir().expect("tempdir");
    // A writable_root that cannot be opened (nonexistent) makes sandbox
    // setup fail; on a build without the backend it fails as Unsupported.
    // Either way the command never runs and the error is typed
    // SandboxSetup — not ExecutionFailed, not a panic.
    let spec = anie_sandbox::SandboxSpec {
        writable_roots: vec![dir.path().join("does-not-exist-xyz")],
        allow_network: false,
        require_kernel_support: true,
    };
    let tool = BashTool::with_sandbox(dir.path(), BashPolicy::default(), Some(spec));
    let err = run_bash(&tool, "echo hi")
        .await
        .expect_err("setup must fail");
    assert!(matches!(err, ToolError::SandboxSetup(_)), "got {err:?}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bash_sandboxed_write_outside_workspace_returns_error_not_panic() {
    let dir = tempdir().expect("tempdir");
    let outside = tempdir().expect("outside");
    let target = outside.path().join("denied.txt");
    let spec = anie_sandbox::SandboxSpec {
        writable_roots: vec![dir.path().to_path_buf()],
        allow_network: true,
        require_kernel_support: true,
    };
    let tool = BashTool::with_sandbox(dir.path(), BashPolicy::default(), Some(spec));
    // Run a command that writes outside the workspace. With Landlock the
    // write is denied (the command exits non-zero); without Landlock the
    // sandbox setup fails closed. Neither path may panic, and the file
    // must not be created.
    let _ = run_bash(&tool, &format!("echo x > {}", target.display())).await;
    assert!(
        !target.exists(),
        "write outside writable roots must be blocked"
    );
}
