use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_stream::stream;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_protocol::{
    AgentEvent, AssistantMessage, ContentBlock, Message, StopReason, ToolCall, ToolDef,
    ToolResult as ProtocolToolResult, Usage, UserMessage,
};
use anie_provider::{
    ApiKind, CostPerMillion, LlmContext, LlmMessage, Model, ModelCompat, Provider, ProviderError,
    ProviderEvent, ProviderRegistry, ProviderStream, RequestOptionsResolver,
    ResolvedRequestOptions, StreamOptions, ThinkingLevel,
    mock::{MockProvider, MockStreamScript},
};

use crate::{
    AgentLoop, AgentLoopConfig, CompactionGate, CompactionGateOutcome, Tool, ToolError,
    ToolExecutionContext, ToolExecutionMode, ToolRegistry,
};

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
        timestamp: 1,
        reasoning_details: None,
    }
}

fn assistant_with_tool_calls(tool_calls: Vec<ToolCall>) -> AssistantMessage {
    let mut content = vec![ContentBlock::Text {
        text: "Need to use a tool".into(),
    }];
    content.extend(tool_calls.into_iter().map(ContentBlock::ToolCall));

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

fn user_prompt(text: &str) -> Message {
    Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        timestamp: 1,
    })
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

struct StaticResolver {
    result: Result<ResolvedRequestOptions, ProviderError>,
}

#[async_trait]
impl RequestOptionsResolver for StaticResolver {
    async fn resolve(
        &self,
        _model: &Model,
        _context: &[Message],
    ) -> Result<ResolvedRequestOptions, ProviderError> {
        self.result.clone()
    }
}

struct TestTool {
    name: String,
    schema: serde_json::Value,
    result_text: String,
    delay: Duration,
    partial_updates: Vec<String>,
    wait_for_cancel: bool,
    fail_execution: bool,
    invocations: Arc<AtomicUsize>,
    current_concurrency: Arc<AtomicUsize>,
    max_concurrency: Arc<AtomicUsize>,
}

impl TestTool {
    fn new(name: &str, schema: serde_json::Value, result_text: &str) -> Self {
        Self {
            name: name.into(),
            schema,
            result_text: result_text.into(),
            delay: Duration::ZERO,
            partial_updates: Vec::new(),
            wait_for_cancel: false,
            fail_execution: false,
            invocations: Arc::new(AtomicUsize::new(0)),
            current_concurrency: Arc::new(AtomicUsize::new(0)),
            max_concurrency: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn with_partial_updates(mut self, partial_updates: Vec<&str>) -> Self {
        self.partial_updates = partial_updates.into_iter().map(str::to_string).collect();
        self
    }

    fn waiting_for_cancel(mut self) -> Self {
        self.wait_for_cancel = true;
        self
    }

    fn failing_execution(mut self) -> Self {
        self.fail_execution = true;
        self
    }
}

struct ConcurrencyGuard {
    current: Arc<AtomicUsize>,
}

impl ConcurrencyGuard {
    fn new(current: Arc<AtomicUsize>, max: Arc<AtomicUsize>) -> Self {
        let current_value = current.fetch_add(1, Ordering::SeqCst) + 1;
        loop {
            let previous = max.load(Ordering::SeqCst);
            if current_value <= previous {
                break;
            }
            if max
                .compare_exchange(previous, current_value, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }
        Self { current }
    }
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        self.current.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Tool for TestTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: self.name.clone(),
            description: format!("{} test tool", self.name),
            parameters: self.schema.clone(),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        update_tx: Option<mpsc::Sender<ProtocolToolResult>>,
        _ctx: &ToolExecutionContext,
    ) -> Result<ProtocolToolResult, ToolError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        let _guard = ConcurrencyGuard::new(
            Arc::clone(&self.current_concurrency),
            Arc::clone(&self.max_concurrency),
        );

        if let Some(update_tx) = update_tx {
            for partial in &self.partial_updates {
                let _ = update_tx
                    .send(ProtocolToolResult {
                        content: vec![ContentBlock::Text {
                            text: partial.clone(),
                        }],
                        details: serde_json::json!({}),
                    })
                    .await;
            }
        }

        if self.wait_for_cancel {
            cancel.cancelled().await;
            return Err(ToolError::Aborted);
        }

        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }

        if self.fail_execution {
            return Err(ToolError::ExecutionFailed(
                "simulated execution failure".into(),
            ));
        }

        let value = args
            .get("value")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(self.result_text.as_str());
        Ok(ProtocolToolResult {
            content: vec![ContentBlock::Text {
                text: format!("{}:{value}", self.result_text),
            }],
            details: serde_json::json!({"value": value}),
        })
    }
}

struct SlowProvider;

impl Provider for SlowProvider {
    fn stream(
        &self,
        _model: &Model,
        _context: LlmContext,
        _options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        let stream = stream! {
            yield Ok(ProviderEvent::Start);
            tokio::time::sleep(Duration::from_millis(25)).await;
            yield Ok(ProviderEvent::TextDelta("partial".into()));
            tokio::time::sleep(Duration::from_secs(5)).await;
            yield Ok(ProviderEvent::Done(final_assistant("partial")));
        };
        Ok(Box::pin(stream))
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
        messages
            .iter()
            .map(|message| LlmMessage {
                role: match message {
                    Message::User(_) => "user",
                    Message::Assistant(_) => "assistant",
                    Message::ToolResult(_) => "tool",
                    Message::Custom(_) => "custom",
                }
                .into(),
                content: serde_json::to_value(message).expect("serialize mock message"),
            })
            .collect()
    }

    fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value> {
        tools
            .iter()
            .map(|tool| serde_json::to_value(tool).expect("serialize tool"))
            .collect()
    }
}

fn agent_with_provider(
    provider: Box<dyn Provider>,
    tool_registry: Arc<ToolRegistry>,
    tool_execution: ToolExecutionMode,
) -> AgentLoop {
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(ApiKind::OpenAICompletions, provider);

    AgentLoop::new(
        Arc::new(provider_registry),
        tool_registry,
        AgentLoopConfig::new(
            sample_model(),
            "You are a test agent".into(),
            ThinkingLevel::Off,
            tool_execution,
            Arc::new(StaticResolver {
                result: Ok(ResolvedRequestOptions::default()),
            }),
        ),
    )
}

async fn collect_run(
    agent: AgentLoop,
    prompts: Vec<Message>,
    context: Vec<Message>,
) -> (crate::AgentRunResult, Vec<AgentEvent>) {
    let (event_tx, mut event_rx) = mpsc::channel(128);
    let cancel = CancellationToken::new();
    let handle = tokio::spawn(async move { agent.run(prompts, context, event_tx, cancel).await });

    let mut events = Vec::new();
    while let Some(event) = event_rx.recv().await {
        let is_end = matches!(event, AgentEvent::AgentEnd { .. });
        events.push(event);
        if is_end {
            break;
        }
    }

    (handle.await.expect("agent task"), events)
}

fn event_kinds(events: &[AgentEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            AgentEvent::AgentStart => "AgentStart",
            AgentEvent::AgentEnd { .. } => "AgentEnd",
            AgentEvent::TurnStart => "TurnStart",
            AgentEvent::TurnEnd { .. } => "TurnEnd",
            AgentEvent::MessageStart { .. } => "MessageStart",
            AgentEvent::MessageDelta { .. } => "MessageDelta",
            AgentEvent::MessageEnd { .. } => "MessageEnd",
            AgentEvent::ToolExecStart { .. } => "ToolExecStart",
            AgentEvent::ToolExecUpdate { .. } => "ToolExecUpdate",
            AgentEvent::ToolExecEnd { .. } => "ToolExecEnd",
            AgentEvent::TranscriptReplace { .. } => "TranscriptReplace",
            AgentEvent::SystemMessage { .. } => "SystemMessage",
            AgentEvent::RlmStatsUpdate { .. } => "RlmStatsUpdate",
            AgentEvent::StatusUpdate { .. } => "StatusUpdate",
            AgentEvent::CompactionStart { .. } => "CompactionStart",
            AgentEvent::CompactionEnd { .. } => "CompactionEnd",
            AgentEvent::RetryScheduled { .. } => "RetryScheduled",
            AgentEvent::EditGuard { .. } => "EditGuard",
            AgentEvent::SessionList { .. } => "SessionList",
        })
        .collect()
}

fn string_arg_tool() -> TestTool {
    TestTool::new(
        "echo",
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            },
            "required": ["value"],
            "additionalProperties": false
        }),
        "echo",
    )
}

#[tokio::test]
async fn basic_flow_prompt_to_assistant_without_tools() {
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![MockStreamScript::from_message(
            final_assistant("done"),
        )])),
        Arc::new(ToolRegistry::new()),
        ToolExecutionMode::Sequential,
    );

    let (result, events) = collect_run(agent, vec![user_prompt("hello")], Vec::new()).await;

    assert_eq!(result.generated_messages.len(), 1);
    assert!(matches!(
        result.generated_messages[0],
        Message::Assistant(AssistantMessage {
            stop_reason: StopReason::Stop,
            ..
        })
    ));
    assert_eq!(
        event_kinds(&events),
        vec![
            "AgentStart",
            "TurnStart",
            "MessageStart",
            "MessageEnd",
            "MessageStart",
            "MessageEnd",
            "TurnEnd",
            "AgentEnd",
        ]
    );
}

#[tokio::test]
async fn single_tool_call_completes_full_loop() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(string_arg_tool()));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "echo",
                serde_json::json!({"value": "first"}),
            )])),
            MockStreamScript::from_message(final_assistant("complete")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    assert_eq!(result.generated_messages.len(), 3);
    assert!(matches!(
        result.generated_messages[1],
        Message::ToolResult(_)
    ));
    assert!(event_kinds(&events).contains(&"ToolExecStart"));
    assert!(event_kinds(&events).contains(&"ToolExecEnd"));
}

fn integer_arg_tool() -> TestTool {
    TestTool::new(
        "count",
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer" }
            },
            "required": ["limit"],
            "additionalProperties": false
        }),
        "count",
    )
}

/// Plan 01 PR 1 of `docs/local_model_augmentation/`: a call
/// whose arguments are mechanically fixable (a stringified
/// number where the schema wants an integer — a routine
/// small-model mistake) is coerced and executed instead of
/// failing validation, and the coercion is surfaced in the
/// result details for the user.
#[tokio::test]
async fn coercion_note_appears_in_tool_result_details() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(integer_arg_tool()));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "count",
                serde_json::json!({"limit": "42"}),
            )])),
            MockStreamScript::from_message(final_assistant("complete")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    let Message::ToolResult(tool_result) = &result.generated_messages[1] else {
        panic!("second generated message must be a tool result");
    };
    assert!(
        !tool_result.is_error,
        "coerced call must execute, got error: {:?}",
        tool_result.content
    );
    let coercions = tool_result
        .details
        .get("argument_coercions")
        .and_then(serde_json::Value::as_array)
        .expect("details carry the coercion notes");
    assert_eq!(coercions.len(), 1, "{coercions:?}");
    assert!(
        coercions[0]
            .as_str()
            .is_some_and(|note| note.contains("limit")),
        "{coercions:?}"
    );
}

/// Regression guard for the coercion seam: arguments that
/// coercion cannot rescue (lossy "4.5" -> integer) must fail
/// with the ORIGINAL validation error text — byte-identical
/// failure behavior to the pre-coercion code path — and must
/// not carry a coercion note.
#[tokio::test]
async fn unrescuable_invalid_arguments_report_the_original_validation_error() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(integer_arg_tool()));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "count",
                serde_json::json!({"limit": "4.5"}),
            )])),
            MockStreamScript::from_message(final_assistant("gave up")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    let Message::ToolResult(tool_result) = &result.generated_messages[1] else {
        panic!("second generated message must be a tool result");
    };
    assert!(tool_result.is_error);
    let ContentBlock::Text { text } = &tool_result.content[0] else {
        panic!("error result must carry text");
    };
    assert!(
        text.contains("Tool arguments failed validation"),
        "original validation error preserved: {text}"
    );
    assert!(
        tool_result.details.get("argument_coercions").is_none(),
        "failed coercion must not leave notes: {:?}",
        tool_result.details
    );
}

/// Agent with the Plan-01-PR-2 repair round enabled. Repair
/// side requests consume MockProvider scripts in order, so
/// each test scripts the repair exchange explicitly.
fn repair_enabled_agent(
    provider: Box<dyn Provider>,
    tool_registry: Arc<ToolRegistry>,
) -> AgentLoop {
    repair_enabled_agent_with_examples(provider, tool_registry, std::collections::HashMap::new())
}

fn repair_enabled_agent_with_examples(
    provider: Box<dyn Provider>,
    tool_registry: Arc<ToolRegistry>,
    examples: std::collections::HashMap<String, String>,
) -> AgentLoop {
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(ApiKind::OpenAICompletions, provider);
    AgentLoop::new(
        Arc::new(provider_registry),
        tool_registry,
        AgentLoopConfig::new(
            sample_model(),
            "You are a test agent".into(),
            ThinkingLevel::Off,
            ToolExecutionMode::Sequential,
            Arc::new(StaticResolver {
                result: Ok(ResolvedRequestOptions::default()),
            }),
        )
        .with_tool_call_repair(true)
        .with_repair_examples(examples),
    )
}

fn first_tool_result(result: &crate::AgentRunResult) -> &anie_protocol::ToolResultMessage {
    result
        .generated_messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(tool_result) => Some(tool_result),
            _ => None,
        })
        .expect("run must produce a tool result")
}

/// Plan 01 PR 2: an invalid call (unrescuable by coercion) is
/// fixed by one focused side request, executes under its
/// ORIGINAL call id, surfaces the round count in details — and
/// the repair exchange never lands in the main context or the
/// main transcript event stream.
#[tokio::test]
async fn invalid_call_triggers_one_repair_request_and_succeeds_on_fix() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(integer_arg_tool()));
    let agent = repair_enabled_agent(
        Box::new(MockProvider::new(vec![
            // Main turn: structurally hopeless arguments.
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "count",
                serde_json::json!({"limit": "lots"}),
            )])),
            // Repair side request: corrected call.
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "repair_1",
                "count",
                serde_json::json!({"limit": 7}),
            )])),
            // Main turn after the tool result.
            MockStreamScript::from_message(final_assistant("complete")),
        ])),
        Arc::new(tools),
    );

    let (result, events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    // Main context: assistant, tool result, assistant — the
    // repair exchange must not appear.
    assert_eq!(result.generated_messages.len(), 3);
    let tool_result = first_tool_result(&result);
    assert!(!tool_result.is_error, "repaired call must execute");
    assert_eq!(
        tool_result.tool_call_id, "call_1",
        "repair must preserve the original call id"
    );
    assert_eq!(
        tool_result.details.get("argument_repair_rounds"),
        Some(&serde_json::Value::from(1u32)),
        "{:?}",
        tool_result.details
    );
    // The repair exchange streams to a drained side channel.
    // If it leaked into the main event stream, some event
    // would reference the side request's "repair_1" call id
    // (the executed call keeps the original "call_1" id).
    assert!(
        events
            .iter()
            .all(|event| !format!("{event:?}").contains("repair_1")),
        "side request must not leak events into the main stream"
    );
}

/// Plan 01 PR 2: when every repair round returns still-invalid
/// arguments, the loop falls back to the synthetic-failure
/// path with the ORIGINAL validation error — byte-identical to
/// repair-disabled behavior — and leaves no repair note.
#[tokio::test]
async fn repair_budget_exhaustion_falls_back_to_synthetic_failure() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(integer_arg_tool()));
    let agent = repair_enabled_agent(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "count",
                serde_json::json!({"limit": "lots"}),
            )])),
            // Two repair rounds, both still invalid.
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "repair_1",
                "count",
                serde_json::json!({"limit": "more"}),
            )])),
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "repair_2",
                "count",
                serde_json::json!({"limit": "most"}),
            )])),
            MockStreamScript::from_message(final_assistant("gave up")),
        ])),
        Arc::new(tools),
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    let tool_result = first_tool_result(&result);
    assert!(tool_result.is_error);
    let ContentBlock::Text { text } = &tool_result.content[0] else {
        panic!("error result must carry text");
    };
    assert!(
        text.contains("Tool arguments failed validation"),
        "original error reported after exhaustion: {text}"
    );
    assert!(
        tool_result.details.get("argument_repair_rounds").is_none(),
        "failed repair must not leave a note: {:?}",
        tool_result.details
    );
}

/// Plan 01 PR 2: a repair response with no tool call (the
/// model replied with commentary) gives up quietly after that
/// round rather than burning the remaining budget.
#[tokio::test]
async fn repair_gives_up_quietly_when_model_replies_with_commentary() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(integer_arg_tool()));
    let agent = repair_enabled_agent(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "count",
                serde_json::json!({"limit": "lots"}),
            )])),
            // Repair response without a tool call.
            MockStreamScript::from_message(final_assistant("sorry, I cannot")),
            MockStreamScript::from_message(final_assistant("done")),
        ])),
        Arc::new(tools),
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    let tool_result = first_tool_result(&result);
    assert!(tool_result.is_error);
}

/// Plan 01 PR 2: with repair off (the default), an invalid
/// call fails immediately — no side request is issued. Guarded
/// by script count: a stray repair request would consume the
/// final script and change the run's shape.
#[tokio::test]
async fn repair_disabled_by_default_reproduces_todays_failure_path() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(integer_arg_tool()));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "count",
                serde_json::json!({"limit": "lots"}),
            )])),
            MockStreamScript::from_message(final_assistant("acknowledged")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    assert_eq!(result.generated_messages.len(), 3);
    let tool_result = first_tool_result(&result);
    assert!(tool_result.is_error);
    let Message::Assistant(final_message) = &result.generated_messages[2] else {
        panic!("run must end with the scripted final assistant");
    };
    assert_eq!(
        final_message.content,
        vec![ContentBlock::Text {
            text: "acknowledged".into()
        }],
        "no script may be consumed by a stray repair request"
    );
}

/// Plan 01 §9b: when the failing tool has a configured example
/// call, the repair side-request prompt carries it so the
/// repairing model imitates a known-good shape. Observed via a
/// provider wrapper that records every request context; the
/// side request is the one advertising exactly one tool.
#[tokio::test]
async fn repair_prompt_includes_canonical_example_when_available() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(integer_arg_tool()));
    // A second registered tool, so the main request advertises
    // two tools and the single-tool context below can only be
    // the repair side request.
    tools.register(Arc::new(string_arg_tool()));
    let contexts: Arc<std::sync::Mutex<Vec<LlmContext>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = RecordingProvider {
        inner: MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "count",
                serde_json::json!({"limit": "lots"}),
            )])),
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "repair_1",
                "count",
                serde_json::json!({"limit": 7}),
            )])),
            MockStreamScript::from_message(final_assistant("complete")),
        ]),
        contexts: Arc::clone(&contexts),
    };
    let agent = repair_enabled_agent_with_examples(
        Box::new(provider),
        Arc::new(tools),
        std::collections::HashMap::from([("count".to_string(), r#"{"limit": 7}"#.to_string())]),
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;
    assert!(!first_tool_result(&result).is_error, "repaired call executes");

    let contexts = contexts.lock().expect("contexts lock");
    let side_request = contexts
        .iter()
        .find(|context| context.tools.len() == 1)
        .expect("the repair side request advertises only the failing tool");
    let messages = serde_json::to_value(
        side_request
            .messages
            .iter()
            .map(|message| message.content.clone())
            .collect::<Vec<_>>(),
    )
    .expect("serialize side-request messages");
    assert!(
        value_contains_string(&messages, "A known-good example call:"),
        "{messages}"
    );
    assert!(
        value_contains_string(&messages, r#"{"limit": 7}"#),
        "{messages}"
    );
}

/// Plan 01 §9b: a repaired call that then fails at EXECUTION
/// keeps the repair-round note and gains the
/// `repair_outcome: executed_with_error` marker, so evals can
/// count repaired-but-worse outcomes (field notes F3: the
/// repair produced a schema-valid call that exited 127).
#[tokio::test]
async fn repaired_then_failed_marker_lands_in_details() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(integer_arg_tool().failing_execution()));
    let agent = repair_enabled_agent(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "count",
                serde_json::json!({"limit": "lots"}),
            )])),
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "repair_1",
                "count",
                serde_json::json!({"limit": 7}),
            )])),
            MockStreamScript::from_message(final_assistant("acknowledged")),
        ])),
        Arc::new(tools),
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    let tool_result = first_tool_result(&result);
    assert!(tool_result.is_error, "execution failure stays an error");
    assert_eq!(
        tool_result.details.get("argument_repair_rounds"),
        Some(&serde_json::Value::from(1u32)),
        "repair note survives the execution failure: {:?}",
        tool_result.details
    );
    assert_eq!(
        tool_result.details.get("repair_outcome"),
        Some(&serde_json::Value::String("executed_with_error".into())),
        "{:?}",
        tool_result.details
    );
}

/// Provider wrapper that records every request's
/// `StreamOptions`, so tests can observe the loop-escalation
/// temperature perturbation (plan 03 §2b).
struct OptionsRecordingProvider {
    inner: MockProvider,
    options: Arc<std::sync::Mutex<Vec<StreamOptions>>>,
}

impl Provider for OptionsRecordingProvider {
    fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        self.options
            .lock()
            .expect("options lock")
            .push(options.clone());
        self.inner.stream(model, context, options)
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
        self.inner.convert_messages(messages)
    }

    fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value> {
        self.inner.convert_tools(tools)
    }
}

/// Agent with the failure-loop detector (and therefore the
/// Signal C similar-call detector) installed and every request's
/// `StreamOptions` recorded.
fn detector_agent_recording_options(
    scripts: Vec<MockStreamScript>,
    tools: ToolRegistry,
    threshold: u32,
) -> (AgentLoop, Arc<std::sync::Mutex<Vec<StreamOptions>>>) {
    let options = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = OptionsRecordingProvider {
        inner: MockProvider::new(scripts),
        options: Arc::clone(&options),
    };
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(ApiKind::OpenAICompletions, Box::new(provider));
    let agent = AgentLoop::new(
        Arc::new(provider_registry),
        Arc::new(tools),
        AgentLoopConfig::new(
            sample_model(),
            "You are a test agent".into(),
            ThinkingLevel::Off,
            ToolExecutionMode::Sequential,
            Arc::new(StaticResolver {
                result: Ok(ResolvedRequestOptions::default()),
            }),
        )
        .with_failure_loop_threshold(Some(threshold)),
    );
    (agent, options)
}

fn all_tool_results(result: &crate::AgentRunResult) -> Vec<&anie_protocol::ToolResultMessage> {
    result
        .generated_messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(tool_result) => Some(tool_result),
            _ => None,
        })
        .collect()
}

fn result_text(tool_result: &anie_protocol::ToolResultMessage) -> String {
    tool_result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Plan 03 §2b: the third identical failure crosses the
/// detector threshold, which appends a harness note to the
/// failed result AND arms the perturbation — the NEXT model
/// request carries the bumped temperature (0.8 assumed base +
/// 0.4 bump, clamped to 1.0). Requests before the crossing
/// stay at the provider default (None).
#[tokio::test]
async fn third_identical_failure_bumps_next_request_temperature() {
    let failing_call = |id: &str| {
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            id,
            "missing",
            serde_json::json!({"command": "ls"}),
        )]))
    };
    let (agent, options) = detector_agent_recording_options(
        vec![
            failing_call("call_a"),
            failing_call("call_b"),
            failing_call("call_c"),
            MockStreamScript::from_message(final_assistant("done")),
        ],
        ToolRegistry::new(),
        3,
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("loop")], Vec::new()).await;

    let options = options.lock().expect("options lock");
    assert_eq!(options.len(), 4);
    for request in &options[..3] {
        assert_eq!(
            request.temperature, None,
            "requests before the crossing keep the provider default"
        );
    }
    assert_eq!(
        options[3].temperature,
        Some(1.0),
        "request after the third identical failure is perturbed"
    );

    // The escalation note rides the third failed result — the
    // SystemMessage event never reaches the model, the note does.
    let tool_results = all_tool_results(&result);
    assert_eq!(tool_results.len(), 3);
    assert!(
        result_text(tool_results[2])
            .contains("[harness note] The last 3 attempts called `missing`"),
        "{:?}",
        tool_results[2].content
    );
    assert!(
        !result_text(tool_results[1]).contains("[harness note] The last"),
        "note must only appear at the crossing"
    );
}

/// Plan 03 §2b: any successful tool call clears the
/// perturbation — the request after the success is back to
/// the provider default.
#[tokio::test]
async fn successful_tool_call_resets_perturbation_to_provider_default() {
    let failing_call = |id: &str| {
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            id,
            "missing",
            serde_json::json!({"command": "ls"}),
        )]))
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(string_arg_tool()));
    let (agent, options) = detector_agent_recording_options(
        vec![
            failing_call("call_a"),
            failing_call("call_b"),
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_c",
                "echo",
                serde_json::json!({"value": "recovered"}),
            )])),
            MockStreamScript::from_message(final_assistant("done")),
        ],
        tools,
        2,
    );

    let (_result, _events) = collect_run(agent, vec![user_prompt("loop")], Vec::new()).await;

    let options = options.lock().expect("options lock");
    assert_eq!(options.len(), 4);
    assert_eq!(
        options[2].temperature,
        Some(1.0),
        "armed after the second identical failure"
    );
    assert_eq!(
        options[3].temperature, None,
        "the successful echo call must clear the perturbation"
    );
}

/// Plan 03 §9 (Signal C), field notes F4: near-duplicate calls
/// that all SUCCEED still build the streak — the third member
/// of the cluster gets the harness note and the
/// `[similar-call warning]` SystemMessage.
#[tokio::test]
async fn successful_but_similar_calls_count_toward_the_streak() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(string_arg_tool()));
    let echo_call = |id: &str, value: &str| {
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            id,
            "echo",
            serde_json::json!({"value": value}),
        )]))
    };
    let (agent, _options) = detector_agent_recording_options(
        vec![
            echo_call("call_a", "what time is it right now"),
            echo_call("call_b", "what time is it right now today"),
            echo_call("call_c", "current time right now what is it"),
            MockStreamScript::from_message(final_assistant("done")),
        ],
        tools,
        3,
    );

    let (result, events) = collect_run(agent, vec![user_prompt("search loop")], Vec::new()).await;

    let tool_results = all_tool_results(&result);
    assert_eq!(tool_results.len(), 3);
    assert!(!tool_results[2].is_error, "all calls succeed");
    assert!(
        result_text(tool_results[2])
            .contains("[harness note] Your last 3 `echo` calls were near-duplicates"),
        "{:?}",
        tool_results[2].content
    );
    assert!(
        !result_text(tool_results[1]).contains("near-duplicates"),
        "streak of 2 stays silent"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::SystemMessage { text } if text.contains("[similar-call warning]")
        )),
        "transcript warning expected at streak 3"
    );
}

/// Plan 03 §9: at streak 5 the similar-call detector arms the
/// SAME perturbation slot as the failure-loop escalation —
/// even though every call succeeded (successes clear the slot
/// BEFORE the similarity observation, so the arming wins).
#[tokio::test]
async fn similar_call_streak_of_five_perturbs_next_request_temperature() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(string_arg_tool()));
    let echo_call = |id: &str| {
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            id,
            "echo",
            serde_json::json!({"value": "what time is it right now"}),
        )]))
    };
    let (agent, options) = detector_agent_recording_options(
        vec![
            echo_call("call_a"),
            echo_call("call_b"),
            echo_call("call_c"),
            echo_call("call_d"),
            echo_call("call_e"),
            MockStreamScript::from_message(final_assistant("done")),
        ],
        tools,
        3,
    );

    let (_result, _events) =
        collect_run(agent, vec![user_prompt("search loop")], Vec::new()).await;

    let options = options.lock().expect("options lock");
    assert_eq!(options.len(), 6);
    for request in &options[..5] {
        assert_eq!(
            request.temperature, None,
            "successful calls keep clearing the slot below streak 5"
        );
    }
    assert_eq!(
        options[5].temperature,
        Some(1.0),
        "streak of five arms the shared perturbation slot"
    );
}

/// Plan 03 §9: Signal C is additive — identical failing args
/// still route through the existing failure-loop detector and
/// fire its `[loop warning]`.
#[tokio::test]
async fn identical_args_still_route_through_the_existing_failure_detector() {
    let failing_call = |id: &str| {
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            id,
            "missing",
            serde_json::json!({"command": "ls"}),
        )]))
    };
    let (agent, _options) = detector_agent_recording_options(
        vec![
            failing_call("call_a"),
            failing_call("call_b"),
            failing_call("call_c"),
            MockStreamScript::from_message(final_assistant("done")),
        ],
        ToolRegistry::new(),
        3,
    );

    let (_result, events) = collect_run(agent, vec![user_prompt("loop")], Vec::new()).await;

    let loop_warnings = events
        .iter()
        .filter(|event| matches!(
            event,
            AgentEvent::SystemMessage { text } if text.contains("[loop warning]")
        ))
        .count();
    assert_eq!(loop_warnings, 1, "failure detector unaffected by Signal C");
}

/// Tool that always fails with the wave-1 A3 text-match error
/// shape: `… did not match anything; closest match: path:a-b`.
struct MatchFailingEditTool {
    message: String,
}

#[async_trait]
impl Tool for MatchFailingEditTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "edit".into(),
            description: "edit stand-in".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        _args: serde_json::Value,
        _cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<ProtocolToolResult>>,
        _ctx: &ToolExecutionContext,
    ) -> Result<ProtocolToolResult, ToolError> {
        Err(ToolError::ExecutionFailed(self.message.clone()))
    }
}

/// Drive two identical failing `edit` calls against a real
/// temp file and return the file path plus the run result.
/// Plan 03 §2c fixture: wrap enabled (the grounding gate) +
/// detector installed (per-file strike storage), threshold
/// high enough that the failure-loop note stays out of the
/// way.
async fn run_two_edit_match_failures() -> (String, crate::AgentRunResult) {
    let path = std::env::temp_dir()
        .join(format!("anie_grounding_{}.txt", uuid::Uuid::new_v4()))
        .to_string_lossy()
        .into_owned();
    std::fs::write(&path, "alpha\nbeta\ngamma\ndelta\n").expect("write fixture");

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MatchFailingEditTool {
        message: format!(
            "edit #0 for {path} did not match anything; closest match: {path}:2-3"
        ),
    }));
    let edit_call = |id: &str| {
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            id,
            "edit",
            serde_json::json!({"path": path, "oldText": "stale", "newText": "fresh"}),
        )]))
    };
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(
        ApiKind::OpenAICompletions,
        Box::new(MockProvider::new(vec![
            edit_call("call_a"),
            edit_call("call_b"),
            MockStreamScript::from_message(final_assistant("done")),
        ])),
    );
    let agent = AgentLoop::new(
        Arc::new(provider_registry),
        Arc::new(tools),
        AgentLoopConfig::new(
            sample_model(),
            "You are a test agent".into(),
            ThinkingLevel::Off,
            ToolExecutionMode::Sequential,
            Arc::new(StaticResolver {
                result: Ok(ResolvedRequestOptions::default()),
            }),
        )
        .with_wrap_failed_tool_results(true)
        .with_failure_loop_threshold(Some(10)),
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("edit loop")], Vec::new()).await;
    let _ = std::fs::remove_file(&path);
    (path, result)
}

/// Plan 03 §2c: the FIRST match failure stays directive-only —
/// don't pay the tokens until a repeat proves the model is
/// stuck.
#[tokio::test]
async fn first_failure_does_not_attach_content() {
    let (_path, result) = run_two_edit_match_failures().await;
    let tool_results = all_tool_results(&result);
    assert_eq!(tool_results.len(), 2);
    let first = result_text(tool_results[0]);
    assert!(
        first.contains("The previous tool call FAILED"),
        "wrap directive expected: {first}"
    );
    assert!(
        !first.contains("Current content of"),
        "no grounding on the first strike: {first}"
    );
}

/// Plan 03 §2c: the second match failure on the same file
/// attaches the file's current content at the closest-match
/// locator, with `path:line` markers.
#[tokio::test]
async fn second_match_failure_on_same_file_attaches_best_match_region() {
    let (path, result) = run_two_edit_match_failures().await;
    let tool_results = all_tool_results(&result);
    let second = result_text(tool_results[1]);
    assert!(
        second.contains(&format!("Current content of {path}:2-3")),
        "{second}"
    );
    assert!(second.contains(&format!("{path}:2: beta")), "{second}");
    assert!(second.contains(&format!("{path}:3: gamma")), "{second}");
    assert!(
        second.contains("Adjust oldText to match these exact bytes."),
        "{second}"
    );
    assert!(
        !second.contains(&format!("{path}:1: alpha")),
        "only the locator span is attached: {second}"
    );
}

/// Plan 03 §2c: the two mitigations compose — wrap directive
/// first, original error in the middle, grounding attachment
/// last.
#[tokio::test]
async fn grounding_composes_after_the_wrap_directive() {
    let (_path, result) = run_two_edit_match_failures().await;
    let tool_results = all_tool_results(&result);
    let blocks: Vec<&str> = tool_results[1]
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(blocks.len() >= 3, "{blocks:?}");
    assert!(
        blocks[0].starts_with("[harness note] The previous tool call FAILED"),
        "directive first: {blocks:?}"
    );
    assert!(
        blocks[1].contains("did not match anything"),
        "original error second: {blocks:?}"
    );
    assert!(
        blocks.last().expect("blocks").contains("Current content of"),
        "grounding last: {blocks:?}"
    );
}

/// Provider wrapper that records every request context, so
/// tests can inspect the repair side-request prompt.
struct RecordingProvider {
    inner: MockProvider,
    contexts: Arc<std::sync::Mutex<Vec<LlmContext>>>,
}

impl Provider for RecordingProvider {
    fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        self.contexts
            .lock()
            .expect("contexts lock")
            .push(context.clone());
        self.inner.stream(model, context, options)
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
        self.inner.convert_messages(messages)
    }

    fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value> {
        self.inner.convert_tools(tools)
    }
}

/// Recursively search a JSON value for any string containing
/// `needle` — message serialization shape doesn't matter.
fn value_contains_string(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::String(text) => text.contains(needle),
        serde_json::Value::Array(items) => {
            items.iter().any(|item| value_contains_string(item, needle))
        }
        serde_json::Value::Object(map) => {
            map.values().any(|item| value_contains_string(item, needle))
        }
        _ => false,
    }
}

#[tokio::test]
async fn multiple_sequential_tool_calls_preserve_order() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(string_arg_tool()));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![
                tool_call("call_1", "echo", serde_json::json!({"value": "one"})),
                tool_call("call_2", "echo", serde_json::json!({"value": "two"})),
            ])),
            MockStreamScript::from_message(final_assistant("done")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, _) = collect_run(agent, vec![user_prompt("go")], Vec::new()).await;

    let tool_results: Vec<_> = result
        .generated_messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(tool_result) => Some(tool_result.tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results, vec!["call_1", "call_2"]);
}

#[tokio::test]
async fn parallel_tool_calls_execute_concurrently() {
    let tool = string_arg_tool().with_delay(Duration::from_millis(75));
    let max_concurrency = Arc::clone(&tool.max_concurrency);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(tool));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![
                tool_call("call_1", "echo", serde_json::json!({"value": "one"})),
                tool_call("call_2", "echo", serde_json::json!({"value": "two"})),
            ])),
            MockStreamScript::from_message(final_assistant("done")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Parallel,
    );

    let _ = collect_run(agent, vec![user_prompt("parallel")], Vec::new()).await;
    assert!(max_concurrency.load(Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn cancellation_during_streaming_returns_aborted_assistant() {
    let agent = agent_with_provider(
        Box::new(SlowProvider),
        Arc::new(ToolRegistry::new()),
        ToolExecutionMode::Sequential,
    );
    let (event_tx, mut event_rx) = mpsc::channel(128);
    let cancel = CancellationToken::new();
    let cancel_for_run = cancel.clone();
    let handle = tokio::spawn(async move {
        agent
            .run(
                vec![user_prompt("cancel")],
                Vec::new(),
                event_tx,
                cancel_for_run,
            )
            .await
    });

    while let Some(event) = event_rx.recv().await {
        if matches!(event, AgentEvent::MessageDelta { .. }) {
            cancel.cancel();
            break;
        }
    }

    let result = handle.await.expect("agent task");
    let assistant = match result.generated_messages.last().expect("assistant message") {
        Message::Assistant(assistant) => assistant,
        other => panic!("expected assistant, got {other:?}"),
    };
    assert_eq!(assistant.stop_reason, StopReason::Aborted);
}

#[tokio::test]
async fn cancellation_during_tool_execution_returns_error_tool_result() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(string_arg_tool().waiting_for_cancel()));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![MockStreamScript::from_message(
            assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "echo",
                serde_json::json!({"value": "slow"}),
            )]),
        )])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (event_tx, mut event_rx) = mpsc::channel(128);
    let cancel = CancellationToken::new();
    let cancel_for_run = cancel.clone();
    let handle = tokio::spawn(async move {
        agent
            .run(
                vec![user_prompt("cancel tool")],
                Vec::new(),
                event_tx,
                cancel_for_run,
            )
            .await
    });

    while let Some(event) = event_rx.recv().await {
        if matches!(event, AgentEvent::ToolExecStart { .. }) {
            cancel.cancel();
            break;
        }
    }

    let result = handle.await.expect("agent task");
    let tool_result = result
        .generated_messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(tool_result) => Some(tool_result),
            _ => None,
        })
        .expect("tool result present");
    assert!(tool_result.is_error);
}

#[tokio::test]
async fn tool_not_found_returns_error_result() {
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_missing",
                "missing",
                serde_json::json!({}),
            )])),
            MockStreamScript::from_message(final_assistant("done")),
        ])),
        Arc::new(ToolRegistry::new()),
        ToolExecutionMode::Sequential,
    );

    let (result, _) = collect_run(agent, vec![user_prompt("missing tool")], Vec::new()).await;
    let tool_result = result
        .generated_messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(tool_result) => Some(tool_result),
            _ => None,
        })
        .expect("tool result present");
    assert!(tool_result.is_error);
    assert_eq!(tool_result.tool_name, "missing");
}

/// Plan 01 §9a: a call whose name differs from a registered
/// tool only by case (or `-` vs `_`) is remapped instead of
/// failing, with a `tool_name_rescued` note in the details.
#[tokio::test]
async fn case_insensitive_tool_name_is_rescued() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(integer_arg_tool()));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "Count",
                serde_json::json!({"limit": 3}),
            )])),
            MockStreamScript::from_message(final_assistant("complete")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    let tool_result = first_tool_result(&result);
    assert!(!tool_result.is_error, "rescued call must execute: {:?}", tool_result.content);
    assert_eq!(
        tool_result.details.get("tool_name_rescued"),
        Some(&serde_json::json!({"from": "Count", "to": "count"})),
        "{:?}",
        tool_result.details
    );
}

/// Plan 01 §9a: hyphen/underscore variants normalize to the
/// same name (`Web-Search` → `web_search`).
#[tokio::test]
async fn hyphenated_name_rescues_to_underscored_registered_tool() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(TestTool::new(
        "web_search",
        serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"],
            "additionalProperties": false
        }),
        "search",
    )));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "Web-Search",
                serde_json::json!({"query": "what time is it"}),
            )])),
            MockStreamScript::from_message(final_assistant("complete")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    let tool_result = first_tool_result(&result);
    assert!(!tool_result.is_error, "{:?}", tool_result.content);
    assert_eq!(
        tool_result.details.get("tool_name_rescued"),
        Some(&serde_json::json!({"from": "Web-Search", "to": "web_search"})),
        "{:?}",
        tool_result.details
    );
}

/// Plan 01 §9a, field notes F1: a hallucinated tool name
/// (`tool_calls` — the wire-format key) whose arguments validate
/// against EXACTLY ONE registered schema is remapped to that
/// tool with a rescue note, instead of burning a turn.
#[tokio::test]
async fn hallucinated_name_with_uniquely_matching_schema_remaps_with_note() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(integer_arg_tool()));
    tools.register(Arc::new(string_arg_tool()));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "tool_calls",
                serde_json::json!({"limit": 3}),
            )])),
            MockStreamScript::from_message(final_assistant("complete")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    let tool_result = first_tool_result(&result);
    assert!(!tool_result.is_error, "{:?}", tool_result.content);
    assert_eq!(
        tool_result.details.get("tool_name_rescued"),
        Some(&serde_json::json!({"from": "tool_calls", "to": "count"})),
        "{:?}",
        tool_result.details
    );
}

/// The literal field-notes F1 call shape (session 0f9cd627): a
/// hallucinated tool name whose arguments ALSO carry a
/// small-model artifact (stringified integer). The schema
/// fingerprint must judge the coerced shape — raw-args
/// validation would refuse exactly the call this rescue was
/// built for. Regression for the 2026-06-12 review finding.
#[tokio::test]
async fn rescue_fingerprint_matches_on_coerced_not_raw_arguments() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(integer_arg_tool()));
    tools.register(Arc::new(string_arg_tool()));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "tool_calls",
                // "3" (not 3): raw validation against the integer
                // schema fails; coerced validation passes.
                serde_json::json!({"limit": "3"}),
            )])),
            MockStreamScript::from_message(final_assistant("complete")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    let tool_result = first_tool_result(&result);
    assert!(
        !tool_result.is_error,
        "rescue + coercion must compose: {:?}",
        tool_result.content
    );
    assert_eq!(
        tool_result.details.get("tool_name_rescued"),
        Some(&serde_json::json!({"from": "tool_calls", "to": "count"})),
        "{:?}",
        tool_result.details
    );
    assert!(
        tool_result.details.get("argument_coercions").is_some(),
        "the stringified integer must be coerced post-rescue: {:?}",
        tool_result.details
    );
}

/// Plan 01 §9a: when the arguments validate against more than
/// one registered schema the rescue refuses to guess; the
/// failure text now lists the registered tool names (the
/// SkillTool unknown-name message is the precedent).
#[tokio::test]
async fn ambiguous_schema_match_fails_with_tool_list_in_error() {
    let echo_schema = serde_json::json!({
        "type": "object",
        "properties": { "value": { "type": "string" } },
        "required": ["value"],
        "additionalProperties": false
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(TestTool::new("echo", echo_schema.clone(), "echo")));
    tools.register(Arc::new(TestTool::new("echo_twin", echo_schema, "twin")));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "tool_calls",
                serde_json::json!({"value": "x"}),
            )])),
            MockStreamScript::from_message(final_assistant("gave up")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    let tool_result = first_tool_result(&result);
    assert!(tool_result.is_error, "ambiguous match must not remap");
    let ContentBlock::Text { text } = &tool_result.content[0] else {
        panic!("error result must carry text");
    };
    assert!(text.contains("Tool not found: tool_calls"), "{text}");
    assert!(text.contains("- echo"), "{text}");
    assert!(text.contains("- echo_twin"), "{text}");
    assert!(
        tool_result.details.get("tool_name_rescued").is_none(),
        "{:?}",
        tool_result.details
    );
}

/// Plan 01 §9a: a rescued call still goes through the normal
/// validation/coercion path — the field session's bash-shaped
/// calls also carried a stringified `"timeout": "5"`.
#[tokio::test]
async fn rescued_call_still_passes_through_coercion() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(integer_arg_tool()));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "Count",
                serde_json::json!({"limit": "42"}),
            )])),
            MockStreamScript::from_message(final_assistant("complete")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    let tool_result = first_tool_result(&result);
    assert!(!tool_result.is_error, "{:?}", tool_result.content);
    assert!(
        tool_result.details.get("tool_name_rescued").is_some(),
        "{:?}",
        tool_result.details
    );
    assert!(
        tool_result.details.get("argument_coercions").is_some(),
        "rescue composes with coercion: {:?}",
        tool_result.details
    );
}

/// Plan 01 §9a safety constraint: schema fingerprinting never
/// remaps a call TO `bash` unless the original name normalizes
/// to bash or the args carry `command` — even when bash's schema
/// is the unique validating match.
#[tokio::test]
async fn schema_fingerprint_refuses_bash_without_command_or_bash_name() {
    // A permissive stand-in bash schema where a command-less
    // object can validate (the real bash requires `command`,
    // making this constraint otherwise untestable).
    let bash_tool = TestTool::new(
        "bash",
        serde_json::json!({
            "type": "object",
            "properties": { "script": { "type": "string" } },
            "required": ["script"],
            "additionalProperties": false
        }),
        "bash",
    );
    let invocations = Arc::clone(&bash_tool.invocations);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(bash_tool));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "tool_calls",
                serde_json::json!({"script": "rm -rf /"}),
            )])),
            MockStreamScript::from_message(final_assistant("gave up")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    let tool_result = first_tool_result(&result);
    assert!(tool_result.is_error, "must not remap to bash");
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "bash must never execute from a refused rescue"
    );
}

/// PR 2 of `docs/harness_mitigations_2026-05-01/`. Drives a
/// model that calls a missing tool with identical args three
/// times in a row. With `failure_loop_threshold = 2`, the
/// detector should fire exactly one `SystemMessage` warning
/// (throttled — the third call doesn't re-warn) and the run
/// should reach `AgentEnd` (observability-only, no abort).
#[tokio::test]
async fn failure_loop_detector_emits_warning_without_aborting() {
    let mut provider_registry = ProviderRegistry::new();
    let scripts = vec![
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_a",
            "missing",
            serde_json::json!({"command": "ls"}),
        )])),
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_b",
            "missing",
            serde_json::json!({"command": "ls"}),
        )])),
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_c",
            "missing",
            serde_json::json!({"command": "ls"}),
        )])),
        MockStreamScript::from_message(final_assistant("done")),
    ];
    provider_registry.register(ApiKind::OpenAICompletions, Box::new(MockProvider::new(scripts)));

    let agent = AgentLoop::new(
        Arc::new(provider_registry),
        Arc::new(ToolRegistry::new()),
        AgentLoopConfig::new(
            sample_model(),
            "You are a test agent".into(),
            ThinkingLevel::Off,
            ToolExecutionMode::Sequential,
            Arc::new(StaticResolver {
                result: Ok(ResolvedRequestOptions::default()),
            }),
        )
        .with_failure_loop_threshold(Some(2)),
    );

    let (result, events) = collect_run(
        agent,
        vec![user_prompt("trigger the loop")],
        Vec::new(),
    )
    .await;

    let loop_warnings: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::SystemMessage { text } if text.contains("[loop warning]") => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        loop_warnings.len(),
        1,
        "detector should fire exactly one warning (throttle), got: {loop_warnings:?}"
    );
    assert!(
        loop_warnings[0].contains("missing"),
        "warning should name the looping tool: {loop_warnings:?}"
    );
    // Run completed normally — observability-only, no abort.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::AgentEnd { .. })),
        "agent should still reach AgentEnd despite the loop"
    );
    let _ = result;
}

/// PR 1 of `docs/rlm_subagents_2026-05-01/`. With no
/// threshold installed, the recurse-depth detector is silent.
#[tokio::test]
async fn recurse_depth_detector_silent_when_threshold_unset() {
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(
        ApiKind::OpenAICompletions,
        Box::new(MockProvider::new(vec![MockStreamScript::from_message(
            final_assistant("done"),
        )])),
    );

    let agent = AgentLoop::new(
        Arc::new(provider_registry),
        Arc::new(ToolRegistry::new()),
        AgentLoopConfig::new(
            sample_model(),
            "agent".into(),
            ThinkingLevel::Off,
            ToolExecutionMode::Sequential,
            Arc::new(StaticResolver {
                result: Ok(ResolvedRequestOptions::default()),
            }),
        ),
    );

    let (_result, events) = collect_run(agent, vec![user_prompt("anything")], Vec::new()).await;
    let depth_warnings: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::SystemMessage { text } if text.contains("[recurse depth]") => Some(text),
            _ => None,
        })
        .collect();
    assert!(
        depth_warnings.is_empty(),
        "no warning expected with threshold=None: {depth_warnings:?}"
    );
}

/// PR 2 of `docs/harness_mitigations_2026-05-01/`. With no
/// threshold installed, the detector is silent — confirms
/// the gate (default off) doesn't accidentally fire.
#[tokio::test]
async fn failure_loop_detector_silent_when_threshold_unset() {
    let mut provider_registry = ProviderRegistry::new();
    let scripts = vec![
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_a",
            "missing",
            serde_json::json!({"command": "ls"}),
        )])),
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_b",
            "missing",
            serde_json::json!({"command": "ls"}),
        )])),
        MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
            "call_c",
            "missing",
            serde_json::json!({"command": "ls"}),
        )])),
        MockStreamScript::from_message(final_assistant("done")),
    ];
    provider_registry.register(ApiKind::OpenAICompletions, Box::new(MockProvider::new(scripts)));

    let agent = AgentLoop::new(
        Arc::new(provider_registry),
        Arc::new(ToolRegistry::new()),
        AgentLoopConfig::new(
            sample_model(),
            "You are a test agent".into(),
            ThinkingLevel::Off,
            ToolExecutionMode::Sequential,
            Arc::new(StaticResolver {
                result: Ok(ResolvedRequestOptions::default()),
            }),
        ),
    );

    let (_result, events) =
        collect_run(agent, vec![user_prompt("no detector")], Vec::new()).await;

    let loop_warnings: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::SystemMessage { text } if text.contains("[loop warning]") => Some(text),
            _ => None,
        })
        .collect();
    assert!(
        loop_warnings.is_empty(),
        "no warning expected when threshold is None: {loop_warnings:?}"
    );
}

#[tokio::test]
async fn tool_argument_validation_failure_skips_execution() {
    let tool = string_arg_tool();
    let invocations = Arc::clone(&tool.invocations);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(tool));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_bad_args",
                "echo",
                serde_json::json!({"not_value": true}),
            )])),
            MockStreamScript::from_message(final_assistant("done")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, _) = collect_run(agent, vec![user_prompt("bad args")], Vec::new()).await;
    assert_eq!(invocations.load(Ordering::SeqCst), 0);
    let tool_result = result
        .generated_messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(tool_result) => Some(tool_result),
            _ => None,
        })
        .expect("tool result present");
    assert!(tool_result.is_error);
}

#[tokio::test]
async fn multiple_turns_with_multiple_tool_round_trips_work() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(string_arg_tool()));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "echo",
                serde_json::json!({"value": "first"}),
            )])),
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_2",
                "echo",
                serde_json::json!({"value": "second"}),
            )])),
            MockStreamScript::from_message(final_assistant("done")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (result, _) = collect_run(agent, vec![user_prompt("two turns")], Vec::new()).await;
    assert_eq!(
        result
            .generated_messages
            .iter()
            .filter(|message| matches!(message, Message::ToolResult(_)))
            .count(),
        2
    );
    assert!(matches!(
        result.generated_messages.last(),
        Some(Message::Assistant(_))
    ));
}

#[tokio::test]
async fn provider_stream_error_is_preserved_and_stops_run() {
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![MockStreamScript::new(vec![
            Ok(ProviderEvent::Start),
            Err(ProviderError::MalformedStreamEvent("boom".into())),
        ])])),
        Arc::new(ToolRegistry::new()),
        ToolExecutionMode::Sequential,
    );

    let (result, events) = collect_run(agent, vec![user_prompt("error")], Vec::new()).await;
    let assistant = match result.generated_messages.last().expect("assistant message") {
        Message::Assistant(assistant) => assistant,
        other => panic!("expected assistant, got {other:?}"),
    };
    assert_eq!(assistant.stop_reason, StopReason::Error);
    assert!(
        assistant
            .error_message
            .as_deref()
            .expect("error message")
            .contains("boom")
    );
    assert!(assistant.content.iter().any(|block| matches!(
        block,
        ContentBlock::Text { text } if text.contains("boom")
    )));
    assert_eq!(
        result.terminal_error,
        Some(ProviderError::MalformedStreamEvent("boom".into()))
    );
    assert!(event_kinds(&events).contains(&"MessageEnd"));
}

#[tokio::test]
async fn tool_partial_updates_emit_tool_exec_update_events() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(
        string_arg_tool().with_partial_updates(vec!["step 1", "step 2"]),
    ));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_updates",
                "echo",
                serde_json::json!({"value": "partial"}),
            )])),
            MockStreamScript::from_message(final_assistant("done")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
    );

    let (_result, events) = collect_run(agent, vec![user_prompt("updates")], Vec::new()).await;
    assert!(event_kinds(&events).contains(&"ToolExecUpdate"));
}

// --------------------------------------------------------------
// PR 8.3 of `docs/midturn_compaction_2026-04-27/`:
// `CompactionGate` hook tests.
// --------------------------------------------------------------

/// Test gate that records every call and returns a configurable
/// outcome. Driven from the test by an `Arc<Mutex<Vec<...>>>`
/// queue so a single agent run can produce a sequence of
/// `Continue` / `Compacted` / `Skipped` / `Err` outcomes in
/// order.
struct ScriptedGate {
    script: std::sync::Mutex<std::collections::VecDeque<GateOutcomeScript>>,
    calls: AtomicUsize,
}

enum GateOutcomeScript {
    Continue,
    Compacted(Vec<Message>),
    Skipped(String),
    Err(String),
}

#[async_trait]
impl CompactionGate for ScriptedGate {
    async fn maybe_compact(
        &self,
        _context: &[Message],
    ) -> Result<CompactionGateOutcome, anyhow::Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let next = self
            .script
            .lock()
            .expect("script lock")
            .pop_front()
            .unwrap_or(GateOutcomeScript::Continue);
        match next {
            GateOutcomeScript::Continue => Ok(CompactionGateOutcome::Continue),
            GateOutcomeScript::Compacted(messages) => {
                Ok(CompactionGateOutcome::Compacted { messages })
            }
            GateOutcomeScript::Skipped(reason) => Ok(CompactionGateOutcome::Skipped { reason }),
            GateOutcomeScript::Err(msg) => Err(anyhow::anyhow!(msg)),
        }
    }
}

fn agent_with_gate(
    provider: Box<dyn Provider>,
    tool_registry: Arc<ToolRegistry>,
    gate: Option<Arc<dyn CompactionGate>>,
) -> AgentLoop {
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(ApiKind::OpenAICompletions, provider);

    AgentLoop::new(
        Arc::new(provider_registry),
        tool_registry,
        AgentLoopConfig::new(
            sample_model(),
            "You are a test agent".into(),
            ThinkingLevel::Off,
            ToolExecutionMode::Sequential,
            Arc::new(StaticResolver {
                result: Ok(ResolvedRequestOptions::default()),
            }),
        )
        .with_compaction_gate(gate),
    )
}

/// Default `compaction_gate: None` is byte-identical to today.
/// The gate-handling branch must be a no-op pure addition; this
/// pins that no extra `TranscriptReplace` / `SystemMessage` /
/// other side-effect leaks into a no-gate run.
#[tokio::test]
async fn agent_run_with_no_gate_behaves_like_today() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(string_arg_tool()));
    let agent = agent_with_gate(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "echo",
                serde_json::json!({"value": "first"}),
            )])),
            MockStreamScript::from_message(final_assistant("complete")),
        ])),
        Arc::new(tools),
        None,
    );

    let (_result, events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    let kinds = event_kinds(&events);
    assert!(
        !kinds.contains(&"TranscriptReplace"),
        "no-gate run must not emit TranscriptReplace from the mid-turn path",
    );
    // The no-gate run sequence: prompt → tool-call assistant →
    // tool result → final assistant → done. Pinning the count
    // catches a regression where the gate branch fires extra
    // events.
    assert_eq!(
        kinds.iter().filter(|k| **k == "TurnStart").count(),
        2,
        "no-gate run emits exactly two TurnStart events (initial + post-tool); got {kinds:?}",
    );
}

/// `Compacted { messages }` replaces `context` and emits
/// `TranscriptReplace`. Drive a multi-iteration tool call and
/// have the gate hand back a fresh context after the first
/// iteration; the next request must build from the replaced
/// context, and the UI side must see exactly one
/// `TranscriptReplace`.
#[tokio::test]
async fn agent_run_calls_compaction_gate_between_iterations() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(string_arg_tool()));

    // The gate hands back a single replacement message — by
    // collapsing the entire turn's context to one user message,
    // we can verify the loop actually used the new context for
    // the next iteration: the next provider call sees what we
    // hand back, not the original prompt + assistant + tool
    // result chain.
    let replacement_summary = user_prompt("[summary] previous turn replaced");
    let gate = Arc::new(ScriptedGate {
        script: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            GateOutcomeScript::Compacted(vec![replacement_summary.clone()]),
        ])),
        calls: AtomicUsize::new(0),
    });

    let agent = agent_with_gate(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "echo",
                serde_json::json!({"value": "first"}),
            )])),
            MockStreamScript::from_message(final_assistant("complete")),
        ])),
        Arc::new(tools),
        Some(gate.clone() as Arc<dyn CompactionGate>),
    );

    let (result, events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    assert_eq!(
        gate.calls.load(Ordering::Relaxed),
        1,
        "the gate is consulted exactly once between iterations",
    );
    assert!(
        event_kinds(&events).contains(&"TranscriptReplace"),
        "Compacted must emit TranscriptReplace so the UI re-renders",
    );
    // The replaced context took effect: the agent saw it and
    // produced the second sampling response (the final
    // assistant). Pin via terminal_error == None and a final
    // assistant message present.
    assert!(result.terminal_error.is_none());
    let final_text = result
        .generated_messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) => Some(
                a.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .unwrap_or_default();
    assert!(final_text.contains("complete"));
}

/// `Skipped { reason }` emits a `SystemMessage` and continues.
/// Pin both: the message text contains the reason, and the run
/// reaches its final assistant.
#[tokio::test]
async fn agent_run_surfaces_skipped_reason_and_continues() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(string_arg_tool()));

    let gate = Arc::new(ScriptedGate {
        script: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            GateOutcomeScript::Skipped("budget exhausted".into()),
        ])),
        calls: AtomicUsize::new(0),
    });

    let agent = agent_with_gate(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "echo",
                serde_json::json!({"value": "first"}),
            )])),
            MockStreamScript::from_message(final_assistant("complete")),
        ])),
        Arc::new(tools),
        Some(gate as Arc<dyn CompactionGate>),
    );

    let (result, events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;

    assert!(result.terminal_error.is_none());
    let saw_skip_message = events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::SystemMessage { text } if text.contains("Skipped mid-turn compaction") && text.contains("budget exhausted")
        )
    });
    assert!(
        saw_skip_message,
        "Skipped must surface the reason via SystemMessage",
    );
}

/// A gate `Err(...)` must NOT kill the turn — the next sampling
/// request may still overflow, and the reactive retry path
/// handles that. Without this, a flaky gate would pre-empt the
/// existing retry/recovery story.
#[tokio::test]
async fn agent_run_continues_when_gate_errors() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(string_arg_tool()));

    let gate = Arc::new(ScriptedGate {
        script: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            GateOutcomeScript::Err("simulated gate failure".into()),
        ])),
        calls: AtomicUsize::new(0),
    });

    let agent = agent_with_gate(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "echo",
                serde_json::json!({"value": "first"}),
            )])),
            MockStreamScript::from_message(final_assistant("complete")),
        ])),
        Arc::new(tools),
        Some(gate as Arc<dyn CompactionGate>),
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("run")], Vec::new()).await;
    assert!(
        result.terminal_error.is_none(),
        "gate failure must not produce a terminal error on the run",
    );
}

// ---- Edit-completion guard (PR 1 of docs/edit_completion_guard/) ----

/// Wraps any provider and counts `stream` calls so tests can
/// assert the classifier side-request did (or did not) fire.
struct CountingProvider {
    inner: Box<dyn Provider>,
    calls: Arc<AtomicUsize>,
}

impl Provider for CountingProvider {
    fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.stream(model, context, options)
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
        self.inner.convert_messages(messages)
    }

    fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value> {
        self.inner.convert_tools(tools)
    }
}

/// A mutating tool (`write`) for guard tests — its successful
/// execution must count as a file mutation. Reuses `TestTool`'s
/// machinery; only the name is load-bearing.
fn write_tool() -> TestTool {
    TestTool::new(
        "write",
        serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "additionalProperties": true
        }),
        "wrote",
    )
}

fn guard_agent(
    provider: Box<dyn Provider>,
    tool_registry: Arc<ToolRegistry>,
    edit_guard_rounds: u32,
    edit_expected: Option<bool>,
) -> AgentLoop {
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(ApiKind::OpenAICompletions, provider);

    AgentLoop::new(
        Arc::new(provider_registry),
        tool_registry,
        AgentLoopConfig::new(
            sample_model(),
            "You are a test agent".into(),
            ThinkingLevel::Off,
            ToolExecutionMode::Sequential,
            Arc::new(StaticResolver {
                result: Ok(ResolvedRequestOptions::default()),
            }),
        )
        .with_edit_guard(edit_guard_rounds)
        .with_edit_expected(edit_expected),
    )
}

fn assistant_messages(result: &crate::AgentRunResult) -> usize {
    result
        .generated_messages
        .iter()
        .filter(|message| matches!(message, Message::Assistant(_)))
        .count()
}

/// The core success path: a run that ends in prose with no edit,
/// when edits are expected and the budget is armed, gets re-
/// engaged with a directive and continues instead of finishing.
#[tokio::test]
async fn guard_fires_and_continues_when_edit_expected_and_no_mutation() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(write_tool()));
    let agent = guard_agent(
        Box::new(MockProvider::new(vec![
            // Turn 1: the model answers in prose, no tool call.
            MockStreamScript::from_message(final_assistant("Here is the answer in prose.")),
            // Turn 2 (after the directive): it edits, then a
            // final message ends the run.
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "write",
                serde_json::json!({"value": "patch"}),
            )])),
            MockStreamScript::from_message(final_assistant("Edited the file.")),
        ])),
        Arc::new(tools),
        1,
        Some(true),
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("fix the bug")], Vec::new()).await;

    assert!(result.terminal_error.is_none());
    // Original prose turn + the re-engaged edit turn + the final
    // turn after the tool result = three assistant messages. A run
    // that finished at the first prose ending would have exactly
    // one.
    assert_eq!(
        assistant_messages(&result),
        3,
        "guard must re-engage the loop for a second model turn",
    );
    assert!(
        result
            .generated_messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_))),
        "the re-engaged turn produced the edit",
    );
}

/// When a file was actually mutated this run, the completion
/// boundary is legitimate: the guard must stay silent even with
/// the budget armed and edits expected.
#[tokio::test]
async fn guard_does_not_fire_when_a_file_was_mutated() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(write_tool()));
    let agent = guard_agent(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_1",
                "write",
                serde_json::json!({"value": "patch"}),
            )])),
            MockStreamScript::from_message(final_assistant("Done editing.")),
        ])),
        Arc::new(tools),
        1,
        Some(true),
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("fix the bug")], Vec::new()).await;

    assert!(result.terminal_error.is_none());
    // Exactly the scripted turns: edit turn + final turn. No third
    // model turn means the guard did not re-engage.
    assert_eq!(assistant_messages(&result), 2);
    assert!(
        !result.generated_messages.iter().any(|message| matches!(
            message,
            Message::User(_)
        )),
        "no directive user message should have been persisted",
    );
}

/// An explicit "no change needed" conclusion suppresses the guard
/// even when no file was edited — the model gave the legitimate
/// out, so the run terminates.
#[tokio::test]
async fn guard_suppressed_by_no_change_needed_conclusion() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(write_tool()));
    let agent = guard_agent(
        Box::new(MockProvider::new(vec![MockStreamScript::from_message(
            final_assistant("I investigated and no change is needed; the code is already correct."),
        )])),
        Arc::new(tools),
        1,
        Some(true),
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("fix the bug")], Vec::new()).await;

    assert!(result.terminal_error.is_none());
    assert_eq!(
        assistant_messages(&result),
        1,
        "the no-change conclusion must terminate the run without re-engaging",
    );
}

/// The guard can never intervene more than `edit_guard_rounds`
/// times: with a budget of 1, two consecutive prose endings yield
/// exactly one directive and then the run is allowed to finish.
#[tokio::test]
async fn guard_bounded_by_round_budget() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(write_tool()));
    let agent = guard_agent(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(final_assistant("First prose ending, no edit.")),
            // After the single directive the model still ends in
            // prose; the budget is spent, so this must terminate.
            MockStreamScript::from_message(final_assistant("Second prose ending, still no edit.")),
        ])),
        Arc::new(tools),
        1,
        Some(true),
    );

    let (result, _events) = collect_run(agent, vec![user_prompt("fix the bug")], Vec::new()).await;

    assert!(result.terminal_error.is_none());
    // Two model turns total: the original and the one re-engaged
    // turn. A third would mean the budget was exceeded.
    assert_eq!(
        assistant_messages(&result),
        2,
        "the guard must intervene at most edit_guard_rounds (1) times",
    );
}

/// An explicit edit-expectation override short-circuits the
/// classifier: no extra provider call is made to judge it.
#[tokio::test]
async fn classifier_no_runs_when_edit_expected_is_explicit() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(write_tool()));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = CountingProvider {
        inner: Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(final_assistant("Prose ending, no edit.")),
            MockStreamScript::from_message(final_assistant("Edited (pretend).")),
        ])),
        calls: Arc::clone(&calls),
    };
    let agent = guard_agent(Box::new(provider), Arc::new(tools), 1, Some(true));

    let (result, _events) = collect_run(agent, vec![user_prompt("fix the bug")], Vec::new()).await;

    assert!(result.terminal_error.is_none());
    // Exactly two model turns and not one more: a classifier side-
    // request would have been a third `stream` call.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "explicit edit_expected must skip the classifier side-request",
    );
}

/// With `edit_expected == None` the guard consults the model-
/// judged classifier. A "no" verdict keeps the guard from firing;
/// a "yes" verdict makes it fire. Both directions are pinned here.
#[tokio::test]
async fn classifier_decides_when_edit_expected_is_none() {
    // Case A: classifier says "no" -> guard stays silent.
    let mut tools_no = ToolRegistry::new();
    tools_no.register(Arc::new(write_tool()));
    let calls_no = Arc::new(AtomicUsize::new(0));
    let agent_no = guard_agent(
        Box::new(CountingProvider {
            inner: Box::new(MockProvider::new(vec![
                MockStreamScript::from_message(final_assistant("Prose ending, no edit.")),
                // Classifier side-request verdict.
                MockStreamScript::from_message(final_assistant("no")),
            ])),
            calls: Arc::clone(&calls_no),
        }),
        Arc::new(tools_no),
        1,
        None,
    );

    let (result_no, _events) =
        collect_run(agent_no, vec![user_prompt("what does this function do?")], Vec::new()).await;
    assert!(result_no.terminal_error.is_none());
    assert_eq!(
        assistant_messages(&result_no),
        1,
        "a 'no' classifier verdict must keep the guard from firing",
    );
    assert_eq!(
        calls_no.load(Ordering::SeqCst),
        2,
        "one model turn plus exactly one classifier side-request",
    );

    // Case B: classifier says "yes" -> guard fires and re-engages.
    let mut tools_yes = ToolRegistry::new();
    tools_yes.register(Arc::new(write_tool()));
    let agent_yes = guard_agent(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(final_assistant("Prose ending, no edit.")),
            // Classifier side-request verdict.
            MockStreamScript::from_message(final_assistant("yes")),
            // Re-engaged turn.
            MockStreamScript::from_message(final_assistant("Now editing.")),
        ])),
        Arc::new(tools_yes),
        1,
        None,
    );

    let (result_yes, _events) =
        collect_run(agent_yes, vec![user_prompt("fix the bug")], Vec::new()).await;
    assert!(result_yes.terminal_error.is_none());
    assert_eq!(
        assistant_messages(&result_yes),
        2,
        "a 'yes' classifier verdict must let the guard re-engage the loop",
    );
}

/// Conservative default (2026-06-14 review): on the model-judged
/// (None) path, an ambiguous/unparseable classifier answer must
/// default to NOT firing — a false-positive guard fire would nag an
/// interactive user mid-question. (The benchmark sets --require-edit
/// and never reaches this path.)
#[tokio::test]
async fn ambiguous_classifier_verdict_does_not_fire_the_guard() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(write_tool()));
    let agent = guard_agent(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(final_assistant("Prose ending, no edit.")),
            // Classifier returns neither yes nor no.
            MockStreamScript::from_message(final_assistant("It depends on context.")),
        ])),
        Arc::new(tools),
        1,
        None,
    );

    let (result, _events) =
        collect_run(agent, vec![user_prompt("what does this function do?")], Vec::new()).await;
    assert!(result.terminal_error.is_none());
    assert_eq!(
        assistant_messages(&result),
        1,
        "an ambiguous classifier verdict must leave the guard silent",
    );
}

/// With the guard disabled (the default `edit_guard_rounds == 0`),
/// a prose-only run produces exactly the pre-guard event sequence
/// — even with `edit_expected` set. The guard must be a pure
/// addition gated entirely on the budget.
#[tokio::test]
async fn guard_disabled_by_default_leaves_loop_byte_identical() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(write_tool()));
    let agent = guard_agent(
        Box::new(MockProvider::new(vec![MockStreamScript::from_message(
            final_assistant("Prose ending, no edit."),
        )])),
        Arc::new(tools),
        0,
        Some(true),
    );

    let (result, events) = collect_run(agent, vec![user_prompt("fix the bug")], Vec::new()).await;

    assert!(result.terminal_error.is_none());
    assert_eq!(assistant_messages(&result), 1);
    assert_eq!(
        event_kinds(&events),
        vec![
            "AgentStart",
            "TurnStart",
            "MessageStart",
            "MessageEnd",
            "MessageStart",
            "MessageEnd",
            "TurnEnd",
            "AgentEnd",
        ],
        "a disabled guard must leave the event stream byte-identical",
    );
}
