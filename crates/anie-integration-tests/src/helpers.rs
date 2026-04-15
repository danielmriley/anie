//! Shared helpers for integration tests.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_agent::{AgentLoop, AgentLoopConfig, AgentRunResult, ToolExecutionMode, ToolRegistry};
use anie_protocol::{
    AgentEvent, AssistantMessage, ContentBlock, Message, StopReason, ToolCall, Usage, UserMessage,
};
use anie_provider::{
    ApiKind, CostPerMillion, Model, ProviderError, ProviderRegistry, RequestOptionsResolver,
    ResolvedRequestOptions, ThinkingLevel, mock::MockProvider,
};
use anie_session::{SessionContext, SessionManager};
use anie_tools::{BashTool, EditTool, FileMutationQueue, ReadTool, WriteTool};

/// A minimal mock model for integration tests.
pub fn sample_model() -> Model {
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

/// A resolver that always succeeds with default (no-auth) options.
pub fn static_resolver() -> Arc<dyn RequestOptionsResolver> {
    Arc::new(StaticResolver)
}

/// Build an `AgentLoop` with the given mock provider and tool registry.
pub fn build_agent(provider: MockProvider, tool_registry: Arc<ToolRegistry>) -> AgentLoop {
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(ApiKind::OpenAICompletions, Box::new(provider));

    AgentLoop::new(
        Arc::new(provider_registry),
        tool_registry,
        AgentLoopConfig {
            model: sample_model(),
            system_prompt: "You are a test agent.".into(),
            thinking: ThinkingLevel::Off,
            tool_execution: ToolExecutionMode::Parallel,
            request_options_resolver: static_resolver(),
            get_steering_messages: None,
            get_follow_up_messages: None,
            before_tool_call_hook: None,
            after_tool_call_hook: None,
        },
    )
}

/// Run the agent loop to completion and collect all emitted events.
pub async fn run_agent_collecting_events(
    agent: AgentLoop,
    prompts: Vec<Message>,
    context: Vec<Message>,
) -> (AgentRunResult, Vec<AgentEvent>) {
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

/// Create a fresh session backed by a temp directory.
pub fn create_temp_session() -> (tempfile::TempDir, SessionManager) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();
    let session = SessionManager::new_session(dir.path(), &cwd).expect("new session");
    (dir, session)
}

/// Persist a prompt and agent result into a new session, then reopen and
/// return the deserialized context. This is the standard
/// "persist → close → reopen → build_context" roundtrip used by many tests.
pub fn persist_and_reopen(cwd: &Path, prompt: &Message, result: &AgentRunResult) -> SessionContext {
    let sessions_dir = cwd.join("sessions");
    let mut session = SessionManager::new_session(&sessions_dir, cwd).expect("new session");
    session.append_message(prompt).expect("persist prompt");
    session
        .append_messages(&result.generated_messages)
        .expect("persist result");

    let session_path = std::fs::read_dir(&sessions_dir)
        .expect("read sessions dir")
        .filter_map(Result::ok)
        .next()
        .expect("session file")
        .path();
    let reopened = SessionManager::open_session(&session_path).expect("reopen");
    reopened.build_context()
}

/// Create a tool registry with all four real tools rooted at the given directory.
pub fn real_tool_registry(cwd: &Path) -> Arc<ToolRegistry> {
    let queue = Arc::new(FileMutationQueue::new());
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ReadTool::new(cwd)));
    registry.register(Arc::new(WriteTool::with_queue(cwd, Arc::clone(&queue))));
    registry.register(Arc::new(EditTool::with_queue(cwd, Arc::clone(&queue))));
    registry.register(Arc::new(BashTool::new(cwd)));
    Arc::new(registry)
}

/// Create a user prompt message.
pub fn user_prompt(text: &str) -> Message {
    Message::User(UserMessage {
        content: vec![ContentBlock::Text { text: text.into() }],
        timestamp: 1,
    })
}

/// Create a user prompt with a specific timestamp.
pub fn user_prompt_at(text: &str, timestamp: u64) -> Message {
    Message::User(UserMessage {
        content: vec![ContentBlock::Text { text: text.into() }],
        timestamp,
    })
}

/// Create a final assistant message with text-only content.
pub fn final_assistant(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text { text: text.into() }],
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 1,
    }
}

/// Create a final assistant message with a specific timestamp.
pub fn final_assistant_at(text: &str, timestamp: u64) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text { text: text.into() }],
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp,
    }
}

/// Create an assistant message that requests tool calls.
pub fn assistant_with_tool_calls(tool_calls: Vec<ToolCall>) -> AssistantMessage {
    let content = tool_calls.into_iter().map(ContentBlock::ToolCall).collect();
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

/// Create a tool call.
pub fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}
