//! Behavior characterization tests for `AgentLoop::run`.
//!
//! These tests lock down the lifecycle and message-accumulation
//! invariants of the current loop. PRs 2-6 of the REPL refactor
//! must preserve them. See
//! `docs/repl_agent_loop/01_behavior_characterization.md`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{collections::VecDeque, sync::Arc};

use anie_agent::{
    AgentLoop, AgentLoopConfig, Tool, ToolError, ToolExecutionContext, ToolExecutionMode,
    ToolRegistry,
};
use anie_protocol::{
    AgentEvent, AssistantMessage, ContentBlock, Message, StopReason, StreamDelta, ToolCall,
    ToolDef, ToolResult, Usage, UserMessage,
};
use anie_provider::{
    ApiKind, CostPerMillion, LlmContext, LlmMessage, Model, ModelCompat, Provider, ProviderError,
    ProviderEvent, ProviderRegistry, ProviderStream, RequestOptionsResolver,
    ResolvedRequestOptions, StreamOptions, ThinkingLevel,
    mock::{MockProvider, MockStreamScript},
};
use async_stream::stream;
use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

// =========================================================================
// Helpers
// =========================================================================

fn sample_model() -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock Model".into(),
        provider: "mock".into(),
        api: ApiKind::OpenAICompletions,
        base_url: "http://localhost".into(),
        context_window: 32_768,
        max_tokens: 8_192,
        supports_reasoning: false,
        reasoning_capabilities: None,
        supports_images: false,
        cost_per_million: CostPerMillion::zero(),
        replay_capabilities: None,
        compat: ModelCompat::None,
    }
}

fn agent_loop_config(
    tool_execution: ToolExecutionMode,
    resolver: Arc<dyn RequestOptionsResolver>,
) -> AgentLoopConfig {
    AgentLoopConfig::new(
        sample_model(),
        "system".into(),
        ThinkingLevel::Off,
        tool_execution,
        resolver,
    )
}

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

struct FailingResolver(ProviderError);

#[async_trait]
impl RequestOptionsResolver for FailingResolver {
    async fn resolve(
        &self,
        _model: &Model,
        _context: &[Message],
    ) -> Result<ResolvedRequestOptions, ProviderError> {
        Err(self.0.clone())
    }
}

fn user_prompt(text: &str) -> Message {
    Message::User(UserMessage {
        content: vec![ContentBlock::Text { text: text.into() }],
        timestamp: 1,
    })
}

fn assistant_text(text: &str, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text { text: text.into() }],
        usage: Usage::default(),
        stop_reason,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 1,
        reasoning_details: None,
    }
}

fn assistant_with_tool_calls(text: &str, calls: Vec<ToolCall>) -> AssistantMessage {
    let mut content = if text.is_empty() {
        Vec::new()
    } else {
        vec![ContentBlock::Text { text: text.into() }]
    };
    for call in calls {
        content.push(ContentBlock::ToolCall(call));
    }
    AssistantMessage {
        content,
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 1,
        reasoning_details: None,
    }
}

fn make_tool_call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

/// Build a standard test rig backed by the workspace `MockProvider`.
fn build_loop(
    scripts: Vec<MockStreamScript>,
    tools: Vec<Arc<dyn Tool>>,
    config: AgentLoopConfig,
) -> AgentLoop {
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(
        ApiKind::OpenAICompletions,
        Box::new(MockProvider::new(scripts)),
    );
    let mut tool_registry = ToolRegistry::new();
    for tool in tools {
        tool_registry.register(tool);
    }
    AgentLoop::new(Arc::new(provider_registry), Arc::new(tool_registry), config)
}

/// Build a rig that uses a caller-supplied provider (e.g. the
/// pausing one used by the cancellation test).
fn build_loop_with_provider(
    provider: Box<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
    config: AgentLoopConfig,
) -> AgentLoop {
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(ApiKind::OpenAICompletions, provider);
    let mut tool_registry = ToolRegistry::new();
    for tool in tools {
        tool_registry.register(tool);
    }
    AgentLoop::new(Arc::new(provider_registry), Arc::new(tool_registry), config)
}

/// Drain all events from a receiver into a Vec.
async fn drain(mut rx: mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

/// Discriminant of an event for compact lifecycle assertions.
fn ev_kind(e: &AgentEvent) -> &'static str {
    match e {
        AgentEvent::AgentStart => "AgentStart",
        AgentEvent::AgentEnd { .. } => "AgentEnd",
        AgentEvent::TurnStart => "TurnStart",
        AgentEvent::TurnEnd { .. } => "TurnEnd",
        AgentEvent::MessageStart { .. } => "MessageStart",
        AgentEvent::MessageEnd { .. } => "MessageEnd",
        AgentEvent::MessageDelta { .. } => "MessageDelta",
        AgentEvent::ToolExecStart { .. } => "ToolExecStart",
        AgentEvent::ToolExecEnd { .. } => "ToolExecEnd",
        AgentEvent::ToolExecUpdate { .. } => "ToolExecUpdate",
        AgentEvent::TranscriptReplace { .. } => "TranscriptReplace",
        AgentEvent::SystemMessage { .. } => "SystemMessage",
        AgentEvent::RlmStatsUpdate { .. } => "RlmStatsUpdate",
        AgentEvent::StatusUpdate { .. } => "StatusUpdate",
        AgentEvent::CompactionStart { .. } => "CompactionStart",
        AgentEvent::CompactionEnd { .. } => "CompactionEnd",
        AgentEvent::RetryScheduled { .. } => "RetryScheduled",
    }
}

/// Filter out streaming deltas (`MessageDelta`) for lifecycle
/// assertions that should not couple to delta chunking.
fn lifecycle_kinds(events: &[AgentEvent]) -> Vec<&'static str> {
    events
        .iter()
        .filter(|e| !matches!(e, AgentEvent::MessageDelta { .. }))
        .map(ev_kind)
        .collect()
}

// =========================================================================
// Test tools
// =========================================================================

fn echo_def() -> ToolDef {
    ToolDef {
        name: "echo".into(),
        description: "echo arg".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"msg": {"type": "string"}},
            "required": ["msg"]
        }),
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> ToolDef {
        echo_def()
    }
    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<ToolResult>>,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let msg = args
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(ToolResult {
            content: vec![ContentBlock::Text {
                text: format!("echo:{msg}"),
            }],
            details: serde_json::json!({"msg": msg}),
        })
    }
}

/// Tool that records the order of completed invocations into a
/// shared Vec — used to verify sequential vs parallel ordering.
struct OrderedEchoTool {
    completions: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for OrderedEchoTool {
    fn definition(&self) -> ToolDef {
        echo_def()
    }
    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<ToolResult>>,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let msg = args
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.completions.lock().await.push(msg.clone());
        Ok(ToolResult {
            content: vec![ContentBlock::Text {
                text: format!("echo:{msg}"),
            }],
            details: serde_json::json!({"msg": msg}),
        })
    }
}

/// Tool that waits for cancellation, then returns Aborted.
struct WaitForCancelTool;

#[async_trait]
impl Tool for WaitForCancelTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "waiter".into(),
            description: "waits for cancellation".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(
        &self,
        _call_id: &str,
        _args: serde_json::Value,
        cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<ToolResult>>,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        cancel.cancelled().await;
        Err(ToolError::Aborted)
    }
}

// =========================================================================
// Pausing test provider
// =========================================================================

/// Provider that yields a single `TextDelta`, then awaits a
/// never-completing future. Used to test cancellation mid-stream:
/// the agent loop's `tokio::select!` will pick the cancel branch
/// once the test cancels the token.
struct PausingProvider;

impl Provider for PausingProvider {
    fn stream(
        &self,
        _model: &Model,
        _context: LlmContext,
        _options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        let event_stream = stream! {
            yield Ok(ProviderEvent::TextDelta("partial".into()));
            // Pause forever — the run is expected to be cancelled
            // before this future resolves.
            futures::future::pending::<()>().await;
            // Unreachable; quiets the unused-result lint.
            yield Ok::<_, ProviderError>(ProviderEvent::TextDelta(String::new()));
        };
        Ok(Box::pin(event_stream))
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
        messages
            .iter()
            .map(|m| LlmMessage {
                role: match m {
                    Message::User(_) => "user",
                    Message::Assistant(_) => "assistant",
                    Message::ToolResult(_) => "tool",
                    Message::Custom(_) => "custom",
                }
                .into(),
                content: serde_json::to_value(m).unwrap_or(serde_json::Value::Null),
            })
            .collect()
    }

    fn convert_tools(&self, _tools: &[ToolDef]) -> Vec<serde_json::Value> {
        Vec::new()
    }
}

// =========================================================================
// Tests
// =========================================================================

/// Test 1 — A no-tool run emits the lifecycle markers in the
/// canonical order with no extras (other than streaming deltas,
/// which we filter out).
#[tokio::test]
async fn run_without_tools_emits_lifecycle_in_order() {
    let scripts = vec![MockStreamScript::new(vec![
        Ok(ProviderEvent::TextDelta("hello".into())),
        Ok(ProviderEvent::Done(assistant_text(
            "hello",
            StopReason::Stop,
        ))),
    ])];
    let agent_loop = build_loop(
        scripts,
        Vec::new(),
        agent_loop_config(ToolExecutionMode::Sequential, Arc::new(StaticResolver)),
    );

    let (tx, rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let _result = agent_loop
        .run(vec![user_prompt("ping")], Vec::new(), tx, cancel)
        .await;

    let events = drain(rx).await;
    assert_eq!(
        lifecycle_kinds(&events),
        vec![
            "AgentStart",
            "TurnStart",
            "MessageStart", // prompt
            "MessageEnd",   // prompt
            "MessageStart", // assistant placeholder
            "MessageEnd",   // assistant final
            "TurnEnd",
            "AgentEnd",
        ]
    );
}

/// Test 2 — Generated messages exclude prompts; final context
/// includes prompts followed by the assistant.
#[tokio::test]
async fn run_without_tools_returns_assistant_in_generated_messages() {
    let scripts = vec![MockStreamScript::from_message(assistant_text(
        "hi",
        StopReason::Stop,
    ))];
    let agent_loop = build_loop(
        scripts,
        Vec::new(),
        agent_loop_config(ToolExecutionMode::Sequential, Arc::new(StaticResolver)),
    );

    let (tx, _rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let result = agent_loop
        .run(vec![user_prompt("hello")], Vec::new(), tx, cancel)
        .await;

    assert_eq!(result.generated_messages.len(), 1);
    assert!(matches!(
        &result.generated_messages[0],
        Message::Assistant(a) if matches!(a.content[0], ContentBlock::Text { ref text } if text == "hi")
    ));
    assert!(result.terminal_error.is_none());
    assert_eq!(result.final_context.len(), 2);
    assert!(matches!(result.final_context[0], Message::User(_)));
    assert!(matches!(result.final_context[1], Message::Assistant(_)));
}

/// Test 3 — One tool call: assistant → ToolExec* → tool result →
/// next TurnStart → next assistant → AgentEnd. Generated messages
/// in that order.
#[tokio::test]
async fn run_with_one_tool_call_appends_assistant_then_tool_result_then_continues() {
    let call = make_tool_call("call_1", "echo", serde_json::json!({"msg": "x"}));
    let scripts = vec![
        MockStreamScript::from_message(assistant_with_tool_calls("", vec![call])),
        MockStreamScript::from_message(assistant_text("done", StopReason::Stop)),
    ];
    let agent_loop = build_loop(
        scripts,
        vec![Arc::new(EchoTool)],
        agent_loop_config(ToolExecutionMode::Sequential, Arc::new(StaticResolver)),
    );

    let (tx, rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let result = agent_loop
        .run(vec![user_prompt("go")], Vec::new(), tx, cancel)
        .await;

    let events = drain(rx).await;
    assert_eq!(
        lifecycle_kinds(&events),
        vec![
            "AgentStart",
            "TurnStart",
            "MessageStart", // prompt
            "MessageEnd",   // prompt
            "MessageStart", // assistant w/ tool call (placeholder)
            "MessageEnd",   // assistant final (Done)
            "ToolExecStart",
            "ToolExecEnd",
            "TurnEnd",      // first turn ends with tool_results populated
            "TurnStart",    // next turn for the follow-up model call
            "MessageStart", // second assistant placeholder
            "MessageEnd",   // second assistant final
            "TurnEnd",
            "AgentEnd",
        ]
    );

    // generated_messages: [assistant_with_tool_call, tool_result, final_assistant]
    assert_eq!(result.generated_messages.len(), 3);
    assert!(matches!(
        result.generated_messages[0],
        Message::Assistant(_)
    ));
    assert!(matches!(
        result.generated_messages[1],
        Message::ToolResult(_)
    ));
    assert!(matches!(
        result.generated_messages[2],
        Message::Assistant(_)
    ));
}

/// Test 4 — Parallel mode returns one result per call. We don't
/// assert order within the batch.
#[tokio::test]
async fn run_with_parallel_tool_calls_returns_one_result_per_call() {
    let calls = vec![
        make_tool_call("c1", "echo", serde_json::json!({"msg": "a"})),
        make_tool_call("c2", "echo", serde_json::json!({"msg": "b"})),
        make_tool_call("c3", "echo", serde_json::json!({"msg": "c"})),
    ];
    let scripts = vec![
        MockStreamScript::from_message(assistant_with_tool_calls("", calls)),
        MockStreamScript::from_message(assistant_text("done", StopReason::Stop)),
    ];
    let agent_loop = build_loop(
        scripts,
        vec![Arc::new(EchoTool)],
        agent_loop_config(ToolExecutionMode::Parallel, Arc::new(StaticResolver)),
    );

    let (tx, _rx) = mpsc::channel(128);
    let cancel = CancellationToken::new();
    let result = agent_loop
        .run(vec![user_prompt("multi")], Vec::new(), tx, cancel)
        .await;

    // generated_messages: [assistant, tr1, tr2, tr3, final_assistant]
    assert_eq!(result.generated_messages.len(), 5);
    let tool_result_ids: Vec<String> = result
        .generated_messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(tr) => Some(tr.tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_result_ids.len(), 3);
    let mut sorted = tool_result_ids;
    sorted.sort();
    assert_eq!(sorted, vec!["c1".to_string(), "c2".into(), "c3".into()]);
}

/// Test 5 — Sequential mode preserves the order of tool calls in
/// the assistant's content. Verified by both completion order
/// (via OrderedEchoTool) and tool_result ordering in
/// generated_messages.
#[tokio::test]
async fn run_with_sequential_tool_calls_preserves_call_order() {
    let calls = vec![
        make_tool_call("c1", "echo", serde_json::json!({"msg": "first"})),
        make_tool_call("c2", "echo", serde_json::json!({"msg": "second"})),
        make_tool_call("c3", "echo", serde_json::json!({"msg": "third"})),
    ];
    let scripts = vec![
        MockStreamScript::from_message(assistant_with_tool_calls("", calls)),
        MockStreamScript::from_message(assistant_text("done", StopReason::Stop)),
    ];
    let completions = Arc::new(Mutex::new(Vec::new()));
    let tool: Arc<dyn Tool> = Arc::new(OrderedEchoTool {
        completions: Arc::clone(&completions),
    });
    let agent_loop = build_loop(
        scripts,
        vec![tool],
        agent_loop_config(ToolExecutionMode::Sequential, Arc::new(StaticResolver)),
    );

    let (tx, _rx) = mpsc::channel(128);
    let cancel = CancellationToken::new();
    let result = agent_loop
        .run(vec![user_prompt("seq")], Vec::new(), tx, cancel)
        .await;

    // Completions captured by the tool itself.
    let actual_completions = completions.lock().await.clone();
    assert_eq!(
        actual_completions,
        vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
        ]
    );

    // tool_result_message ordering in generated_messages matches
    // the call order.
    let ids: Vec<String> = result
        .generated_messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(tr) => Some(tr.tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["c1".to_string(), "c2".into(), "c3".into()]);
}

/// Test 6 — Provider stream error: the assistant has
/// `StopReason::Error` and `terminal_error` is `Some`.
#[tokio::test]
async fn provider_stream_error_returns_error_assistant_with_terminal_error() {
    let err = ProviderError::Transport("kaboom".into());
    let scripts = vec![MockStreamScript::from_error(err.clone())];
    let agent_loop = build_loop(
        scripts,
        Vec::new(),
        agent_loop_config(ToolExecutionMode::Sequential, Arc::new(StaticResolver)),
    );

    let (tx, rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let result = agent_loop
        .run(vec![user_prompt("p")], Vec::new(), tx, cancel)
        .await;

    let events = drain(rx).await;
    let kinds = lifecycle_kinds(&events);
    assert!(
        kinds.last().copied() == Some("AgentEnd"),
        "expected AgentEnd at tail, got {kinds:?}"
    );

    assert_eq!(result.terminal_error, Some(err));
    let assistant = match result.generated_messages.last() {
        Some(Message::Assistant(a)) => a.clone(),
        other => panic!("expected assistant, got {other:?}"),
    };
    assert_eq!(assistant.stop_reason, StopReason::Error);
}

/// Test 7 — Cancellation mid-stream: the assistant is
/// `StopReason::Aborted` and `terminal_error` is `None`.
#[tokio::test]
async fn cancel_during_provider_stream_returns_aborted_assistant_without_terminal_error() {
    let agent_loop = build_loop_with_provider(
        Box::new(PausingProvider),
        Vec::new(),
        agent_loop_config(ToolExecutionMode::Sequential, Arc::new(StaticResolver)),
    );

    let (tx, mut rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();

    // Spawn the run, then drain events until we see a TextDelta;
    // that proves the stream is past the first event and now
    // parked on `pending::<()>().await` inside the provider
    // stream. Cancel at that point — `tokio::select!` is then
    // guaranteed to pick the cancel branch on its next poll.
    let run = tokio::spawn(async move {
        agent_loop
            .run(vec![user_prompt("pause")], Vec::new(), tx, cancel)
            .await
    });

    // Collect events until we observe a TextDelta payload.
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        let saw_text_delta = matches!(
            &event,
            AgentEvent::MessageDelta {
                delta: StreamDelta::TextDelta(_)
            }
        );
        events.push(event);
        if saw_text_delta {
            cancel_for_task.cancel();
            break;
        }
    }
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let result = run.await.expect("run task panicked");

    assert!(result.terminal_error.is_none());
    let last_assistant = result
        .generated_messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) => Some(a.clone()),
            _ => None,
        })
        .expect("assistant in generated_messages");
    assert_eq!(last_assistant.stop_reason, StopReason::Aborted);
}

/// Test 8 — Cancellation during tool execution: the run finishes
/// cleanly. The tool sees the cancel, returns Aborted, the loop
/// notices the cancel post-tool and emits AgentEnd without
/// looping for another model turn.
#[tokio::test]
async fn cancel_during_tool_execution_finishes_run_cleanly() {
    let call = make_tool_call("call_1", "waiter", serde_json::json!({}));
    let scripts = vec![MockStreamScript::from_message(assistant_with_tool_calls(
        "",
        vec![call],
    ))];
    let agent_loop = build_loop(
        scripts,
        vec![Arc::new(WaitForCancelTool)],
        agent_loop_config(ToolExecutionMode::Sequential, Arc::new(StaticResolver)),
    );

    let (tx, mut rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();

    let run = tokio::spawn(async move {
        agent_loop
            .run(vec![user_prompt("wait")], Vec::new(), tx, cancel)
            .await
    });

    // Cancel as soon as ToolExecStart is observed — the tool is
    // then awaiting on the cancel token.
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        let saw_tool_start = matches!(&event, AgentEvent::ToolExecStart { .. });
        events.push(event);
        if saw_tool_start {
            cancel_for_task.cancel();
            break;
        }
    }
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let result = run.await.expect("run task panicked");

    assert!(result.terminal_error.is_none());
    let kinds = lifecycle_kinds(&events);
    assert_eq!(
        kinds.last().copied(),
        Some("AgentEnd"),
        "expected AgentEnd at tail, got {kinds:?}"
    );
    // No second-turn TurnStart should follow ToolExecEnd before
    // AgentEnd. Walk backward from the end and assert.
    let agent_end_idx = kinds.iter().rposition(|k| *k == "AgentEnd").unwrap();
    let after_tool_end_idx = kinds.iter().rposition(|k| *k == "ToolExecEnd").unwrap();
    let between = &kinds[after_tool_end_idx + 1..agent_end_idx];
    assert!(
        !between.contains(&"TurnStart"),
        "no further TurnStart should appear between ToolExecEnd and AgentEnd; got {between:?}"
    );
}

/// Test 9 — Missing provider returns an error assistant with
/// `terminal_error: None` (controller does not retry on this).
#[tokio::test]
async fn missing_provider_returns_error_assistant_without_terminal_error() {
    let provider_registry = ProviderRegistry::new(); // empty
    let tool_registry = ToolRegistry::new();
    let agent_loop = AgentLoop::new(
        Arc::new(provider_registry),
        Arc::new(tool_registry),
        agent_loop_config(ToolExecutionMode::Sequential, Arc::new(StaticResolver)),
    );

    let (tx, _rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let result = agent_loop
        .run(vec![user_prompt("hi")], Vec::new(), tx, cancel)
        .await;

    assert!(result.terminal_error.is_none());
    let last_assistant = result
        .generated_messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) => Some(a.clone()),
            _ => None,
        })
        .expect("error assistant present");
    assert_eq!(last_assistant.stop_reason, StopReason::Error);
}

/// Test 10 — Resolver failure populates `terminal_error` so the
/// controller's retry policy can fire.
#[tokio::test]
async fn request_options_resolution_failure_returns_terminal_error() {
    let err = ProviderError::Auth("nope".into());
    let resolver: Arc<dyn RequestOptionsResolver> = Arc::new(FailingResolver(err.clone()));
    // Provider doesn't matter — resolution fails before the call.
    let agent_loop = build_loop(
        vec![MockStreamScript::from_message(assistant_text(
            "unused",
            StopReason::Stop,
        ))],
        Vec::new(),
        agent_loop_config(ToolExecutionMode::Sequential, resolver),
    );

    let (tx, _rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let result = agent_loop
        .run(vec![user_prompt("hi")], Vec::new(), tx, cancel)
        .await;

    assert_eq!(result.terminal_error, Some(err));
    let last_assistant = result
        .generated_messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) => Some(a.clone()),
            _ => None,
        })
        .expect("error assistant present");
    assert_eq!(last_assistant.stop_reason, StopReason::Error);
}

/// Test 11 — Follow-up messages from a configured provider land
/// in the context before the next TurnStart.
#[tokio::test]
async fn follow_up_messages_append_and_start_next_turn() {
    let scripts = vec![
        MockStreamScript::from_message(assistant_text("first", StopReason::Stop)),
        MockStreamScript::from_message(assistant_text("after-followup", StopReason::Stop)),
    ];
    // Use a one-shot follow-up: yield messages on the first call,
    // empty thereafter. Otherwise the loop runs forever.
    let calls_remaining = Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let calls_for_closure = Arc::clone(&calls_remaining);
    let follow_up: Arc<dyn Fn() -> Vec<Message> + Send + Sync> = Arc::new(move || {
        if calls_for_closure
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |v| if v == 0 { None } else { Some(v - 1) },
            )
            .is_ok()
        {
            vec![Message::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: "follow-up nudge".into(),
                }],
                timestamp: 2,
            })]
        } else {
            Vec::new()
        }
    });
    let config = agent_loop_config(ToolExecutionMode::Sequential, Arc::new(StaticResolver))
        .with_follow_up_messages(follow_up);
    let agent_loop = build_loop(scripts, Vec::new(), config);

    let (tx, rx) = mpsc::channel(128);
    let cancel = CancellationToken::new();
    let result = agent_loop
        .run(vec![user_prompt("p")], Vec::new(), tx, cancel)
        .await;

    let events = drain(rx).await;
    let kinds = lifecycle_kinds(&events);
    // After the first assistant ends, we expect TurnEnd, then
    // TurnStart for the follow-up turn (no intervening AgentEnd).
    let first_turn_end = kinds.iter().position(|k| *k == "TurnEnd").unwrap();
    let next_turn_start = kinds[first_turn_end + 1..]
        .iter()
        .position(|k| *k == "TurnStart")
        .map(|i| i + first_turn_end + 1)
        .unwrap();
    let between = &kinds[first_turn_end + 1..next_turn_start];
    assert!(
        !between.contains(&"AgentEnd"),
        "no AgentEnd should appear between TurnEnd and the follow-up TurnStart; got {between:?}"
    );
    assert_eq!(kinds.last().copied(), Some("AgentEnd"));

    // final_context must include the follow-up message.
    let has_follow_up = result.final_context.iter().any(|m| matches!(m,
        Message::User(u) if matches!(&u.content[0], ContentBlock::Text { text } if text == "follow-up nudge")
    ));
    assert!(has_follow_up, "follow-up message present in final_context");

    // generated_messages excludes the follow-up (it is not
    // assistant- or tool-generated).
    assert!(
        !result.generated_messages.iter().any(|m| matches!(m,
            Message::User(u) if matches!(&u.content[0], ContentBlock::Text { text } if text == "follow-up nudge")
        )),
        "follow-up message must not appear in generated_messages"
    );
}

/// Test 12 — Steering messages from a configured provider appear
/// in `final_context` after tool results, and not in
/// `generated_messages`.
#[tokio::test]
async fn steering_messages_append_after_tool_results_before_next_turn_start() {
    let call = make_tool_call("call_1", "echo", serde_json::json!({"msg": "x"}));
    let scripts = vec![
        MockStreamScript::from_message(assistant_with_tool_calls("", vec![call])),
        MockStreamScript::from_message(assistant_text("post", StopReason::Stop)),
    ];
    let calls_remaining = Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let calls_for_closure = Arc::clone(&calls_remaining);
    let steering: Arc<dyn Fn() -> Vec<Message> + Send + Sync> = Arc::new(move || {
        if calls_for_closure
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |v| if v == 0 { None } else { Some(v - 1) },
            )
            .is_ok()
        {
            vec![Message::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: "steering".into(),
                }],
                timestamp: 3,
            })]
        } else {
            Vec::new()
        }
    });
    let config = agent_loop_config(ToolExecutionMode::Sequential, Arc::new(StaticResolver))
        .with_steering_messages(steering);
    let agent_loop = build_loop(scripts, vec![Arc::new(EchoTool)], config);

    let (tx, _rx) = mpsc::channel(128);
    let cancel = CancellationToken::new();
    let result = agent_loop
        .run(vec![user_prompt("go")], Vec::new(), tx, cancel)
        .await;

    // Walk final_context: prompt, assistant, tool_result, steering, assistant
    let kinds: Vec<&'static str> = result
        .final_context
        .iter()
        .map(|m| match m {
            Message::User(_) => "User",
            Message::Assistant(_) => "Assistant",
            Message::ToolResult(_) => "ToolResult",
            Message::Custom(_) => "Custom",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["User", "Assistant", "ToolResult", "User", "Assistant"]
    );

    // generated_messages excludes the steering User entry.
    let gen_kinds: Vec<&'static str> = result
        .generated_messages
        .iter()
        .map(|m| match m {
            Message::User(_) => "User",
            Message::Assistant(_) => "Assistant",
            Message::ToolResult(_) => "ToolResult",
            Message::Custom(_) => "Custom",
        })
        .collect();
    assert_eq!(
        gen_kinds,
        vec!["Assistant", "ToolResult", "Assistant"],
        "generated_messages excludes steering"
    );
}

/// Test 13 — A multi-step run yields generated_messages in the
/// order they were emitted: assistant_with_tool, tool_result,
/// final_assistant.
#[tokio::test]
async fn generated_messages_order_matches_emission_order() {
    let call = make_tool_call("c1", "echo", serde_json::json!({"msg": "first"}));
    let scripts = vec![
        MockStreamScript::from_message(assistant_with_tool_calls("", vec![call])),
        MockStreamScript::from_message(assistant_text("final", StopReason::Stop)),
    ];
    let agent_loop = build_loop(
        scripts,
        vec![Arc::new(EchoTool)],
        agent_loop_config(ToolExecutionMode::Sequential, Arc::new(StaticResolver)),
    );

    let (tx, _rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let result = agent_loop
        .run(vec![user_prompt("p")], Vec::new(), tx, cancel)
        .await;

    let kinds: Vec<&'static str> = result
        .generated_messages
        .iter()
        .map(|m| match m {
            Message::User(_) => "User",
            Message::Assistant(_) => "Assistant",
            Message::ToolResult(_) => "ToolResult",
            Message::Custom(_) => "Custom",
        })
        .collect();
    assert_eq!(kinds, vec!["Assistant", "ToolResult", "Assistant"]);
}

/// Test 14 — `final_context` equals `prompts` followed by the
/// generated messages, when no follow-up or steering is
/// configured.
#[tokio::test]
async fn final_context_includes_prompts_and_all_generated_in_order() {
    let scripts = vec![MockStreamScript::from_message(assistant_text(
        "ok",
        StopReason::Stop,
    ))];
    let agent_loop = build_loop(
        scripts,
        Vec::new(),
        agent_loop_config(ToolExecutionMode::Sequential, Arc::new(StaticResolver)),
    );

    let (tx, _rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let prompts = vec![user_prompt("a"), user_prompt("b")];
    let result = agent_loop
        .run(prompts.clone(), Vec::new(), tx, cancel)
        .await;

    // prompts (2) + assistant (1) = 3 messages
    assert_eq!(result.final_context.len(), 3);
    let mut expected: VecDeque<Message> = prompts.into_iter().collect();
    for generated in &result.generated_messages {
        expected.push_back(generated.clone());
    }
    let expected: Vec<Message> = expected.into_iter().collect();
    assert_eq!(result.final_context, expected);
}
