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
    ApiKind, CostPerMillion, LlmContext, LlmMessage, Model, Provider, ProviderError, ProviderEvent,
    ProviderRegistry, ProviderStream, RequestOptionsResolver, ResolvedRequestOptions,
    StreamOptions, ThinkingLevel,
    mock::{MockProvider, MockStreamScript},
};

use crate::hooks::{
    AfterToolCallHook, BeforeToolCallHook, BeforeToolCallResult, ToolResultOverride,
};
use crate::{AgentLoop, AgentLoopConfig, Tool, ToolError, ToolExecutionMode, ToolRegistry};

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

struct BlockingHook;

#[async_trait]
impl BeforeToolCallHook for BlockingHook {
    async fn before_tool_call(
        &self,
        _tool_call: &ToolCall,
        _args: &serde_json::Value,
        _context: &[Message],
    ) -> BeforeToolCallResult {
        BeforeToolCallResult::Block {
            reason: "blocked by hook".into(),
        }
    }
}

struct OverrideHook;

#[async_trait]
impl AfterToolCallHook for OverrideHook {
    async fn after_tool_call(
        &self,
        _tool_call: &ToolCall,
        _result: &ProtocolToolResult,
        _is_error: bool,
    ) -> Option<ToolResultOverride> {
        Some(ToolResultOverride {
            content: Some(vec![ContentBlock::Text {
                text: "overridden".into(),
            }]),
            details: Some(serde_json::json!({"overridden": true})),
            is_error: Some(false),
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
    before_hook: Option<Arc<dyn BeforeToolCallHook>>,
    after_hook: Option<Arc<dyn AfterToolCallHook>>,
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
        )
        .with_hooks(before_hook, after_hook),
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
            AgentEvent::StatusUpdate { .. } => "StatusUpdate",
            AgentEvent::CompactionStart => "CompactionStart",
            AgentEvent::CompactionEnd { .. } => "CompactionEnd",
            AgentEvent::RetryScheduled { .. } => "RetryScheduled",
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
        None,
        None,
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
        None,
        None,
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
        None,
        None,
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
        None,
        None,
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
        None,
        None,
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
        None,
        None,
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
        None,
        None,
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
        None,
        None,
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
async fn before_tool_call_hook_can_block_execution() {
    let tool = string_arg_tool();
    let invocations = Arc::clone(&tool.invocations);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(tool));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_blocked",
                "echo",
                serde_json::json!({"value": "blocked"}),
            )])),
            MockStreamScript::from_message(final_assistant("done")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
        Some(Arc::new(BlockingHook)),
        None,
    );

    let (result, _) = collect_run(agent, vec![user_prompt("block")], Vec::new()).await;
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
async fn after_tool_call_hook_can_override_result() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(string_arg_tool()));
    let agent = agent_with_provider(
        Box::new(MockProvider::new(vec![
            MockStreamScript::from_message(assistant_with_tool_calls(vec![tool_call(
                "call_override",
                "echo",
                serde_json::json!({"value": "original"}),
            )])),
            MockStreamScript::from_message(final_assistant("done")),
        ])),
        Arc::new(tools),
        ToolExecutionMode::Sequential,
        None,
        Some(Arc::new(OverrideHook)),
    );

    let (result, _) = collect_run(agent, vec![user_prompt("override")], Vec::new()).await;
    let tool_result = result
        .generated_messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(tool_result) => Some(tool_result),
            _ => None,
        })
        .expect("tool result present");

    assert_eq!(
        tool_result.content,
        vec![ContentBlock::Text {
            text: "overridden".into(),
        }]
    );
    assert!(!tool_result.is_error);
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
        None,
        None,
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
        None,
        None,
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
        None,
        None,
    );

    let (_result, events) = collect_run(agent, vec![user_prompt("updates")], Vec::new()).await;
    assert!(event_kinds(&events).contains(&"ToolExecUpdate"));
}
