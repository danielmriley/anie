//! Tests for the public `AgentRunMachine` API.
//!
//! Plan: `docs/repl_agent_loop/05_step_machine_api.md`. These
//! tests exercise the step-driven entry point directly to confirm
//! it produces the same observable behavior as the
//! run-to-completion `AgentLoop::run` wrapper.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use anie_agent::{AgentLoop, AgentLoopConfig, AgentStepBoundary, ToolExecutionMode, ToolRegistry};
use anie_protocol::ToolDef;
use anie_protocol::{
    AgentEvent, AssistantMessage, ContentBlock, Message, StopReason, StreamDelta, Usage,
    UserMessage,
};
use anie_provider::{
    ApiKind, CostPerMillion, LlmContext, LlmMessage, Model, ModelCompat, Provider, ProviderError,
    ProviderEvent, ProviderRegistry, ProviderStream, RequestOptionsResolver,
    ResolvedRequestOptions, StreamOptions, ThinkingLevel,
    mock::{MockProvider, MockStreamScript},
};
use async_stream::stream;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// =========================================================================
// Helpers (mirrored from agent_loop_behavior.rs to keep this file
// self-contained — the helpers are small and copying avoids
// exposing an internal test-util feature.)
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

fn agent_loop_config() -> AgentLoopConfig {
    AgentLoopConfig::new(
        sample_model(),
        "system".into(),
        ThinkingLevel::Off,
        ToolExecutionMode::Sequential,
        Arc::new(StaticResolver),
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

fn build_loop_with_scripts(scripts: Vec<MockStreamScript>) -> AgentLoop {
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(
        ApiKind::OpenAICompletions,
        Box::new(MockProvider::new(scripts)),
    );
    let tool_registry = ToolRegistry::new();
    AgentLoop::new(
        Arc::new(provider_registry),
        Arc::new(tool_registry),
        agent_loop_config(),
    )
}

fn build_loop_with_provider(provider: Box<dyn Provider>) -> AgentLoop {
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(ApiKind::OpenAICompletions, provider);
    let tool_registry = ToolRegistry::new();
    AgentLoop::new(
        Arc::new(provider_registry),
        Arc::new(tool_registry),
        agent_loop_config(),
    )
}

async fn drain_into(rx: &mut mpsc::Receiver<AgentEvent>, sink: &mut Vec<AgentEvent>) {
    while let Ok(event) = rx.try_recv() {
        sink.push(event);
    }
}

async fn drain_remaining(rx: &mut mpsc::Receiver<AgentEvent>, sink: &mut Vec<AgentEvent>) {
    while let Some(event) = rx.recv().await {
        sink.push(event);
    }
}

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

fn lifecycle_kinds(events: &[AgentEvent]) -> Vec<&'static str> {
    events
        .iter()
        .filter(|e| !matches!(e, AgentEvent::MessageDelta { .. }))
        .map(ev_kind)
        .collect()
}

// =========================================================================
// PausingProvider — yields one TextDelta then awaits forever.
// Used to test partial-result `finish` behavior.
// =========================================================================

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
            futures::future::pending::<()>().await;
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

/// Test 1 — Driving the machine step-by-step produces the same
/// `AgentRunResult` and event order as the run-to-completion
/// `AgentLoop::run` wrapper.
#[tokio::test]
async fn step_machine_one_step_at_a_time_matches_run_to_completion() {
    // Step-by-step path
    let scripts_step = vec![MockStreamScript::from_message(assistant_text(
        "hi",
        StopReason::Stop,
    ))];
    let loop_step = build_loop_with_scripts(scripts_step);
    let (tx_step, mut rx_step) = mpsc::channel(64);
    let cancel_step = CancellationToken::new();
    let mut machine = loop_step
        .start_run_machine(vec![user_prompt("hello")], Vec::new(), &tx_step)
        .await;
    while !machine.is_finished() {
        machine.next_step(&tx_step, &cancel_step).await;
    }
    let result_step = machine.finish(&tx_step).await;
    drop(tx_step);
    let mut events_step = Vec::new();
    drain_remaining(&mut rx_step, &mut events_step).await;

    // Wrapper path
    let scripts_run = vec![MockStreamScript::from_message(assistant_text(
        "hi",
        StopReason::Stop,
    ))];
    let loop_run = build_loop_with_scripts(scripts_run);
    let (tx_run, mut rx_run) = mpsc::channel(64);
    let cancel_run = CancellationToken::new();
    let result_run = loop_run
        .run(vec![user_prompt("hello")], Vec::new(), tx_run, cancel_run)
        .await;
    let mut events_run = Vec::new();
    drain_remaining(&mut rx_run, &mut events_run).await;

    assert_eq!(
        result_step.generated_messages,
        result_run.generated_messages
    );
    assert_eq!(result_step.final_context, result_run.final_context);
    assert_eq!(result_step.terminal_error, result_run.terminal_error);
    assert_eq!(lifecycle_kinds(&events_step), lifecycle_kinds(&events_run));
}

/// Test 2 — Run-start events (`AgentStart`, the initial
/// `TurnStart`, prompt `MessageStart`/`MessageEnd`) are emitted
/// *before* `start_run_machine` returns, so they can be drained
/// before any `next_step` call.
#[tokio::test]
async fn step_machine_emits_run_start_events_before_first_step() {
    let scripts = vec![MockStreamScript::from_message(assistant_text(
        "ok",
        StopReason::Stop,
    ))];
    let agent_loop = build_loop_with_scripts(scripts);
    let (tx, mut rx) = mpsc::channel(64);
    let _machine = agent_loop
        .start_run_machine(vec![user_prompt("hello")], Vec::new(), &tx)
        .await;

    // Drain the channel (without awaiting more events from the
    // not-yet-started machine).
    let mut events = Vec::new();
    drain_into(&mut rx, &mut events).await;
    let kinds = lifecycle_kinds(&events);
    assert_eq!(
        kinds,
        vec!["AgentStart", "TurnStart", "MessageStart", "MessageEnd",]
    );
}

/// Test 3 — After the only model turn returns a clean assistant
/// with no tool calls, `is_finished()` is true and the next
/// `next_step` is a no-op returning `Finished`.
#[tokio::test]
async fn step_machine_is_finished_after_terminal_observation() {
    let scripts = vec![MockStreamScript::from_message(assistant_text(
        "ok",
        StopReason::Stop,
    ))];
    let agent_loop = build_loop_with_scripts(scripts);
    let (tx, _rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let mut machine = agent_loop
        .start_run_machine(vec![user_prompt("p")], Vec::new(), &tx)
        .await;

    let boundary = machine.next_step(&tx, &cancel).await;
    assert_eq!(boundary, AgentStepBoundary::Finished);
    assert!(machine.is_finished());
    // Subsequent next_step is a no-op that returns Finished.
    let boundary2 = machine.next_step(&tx, &cancel).await;
    assert_eq!(boundary2, AgentStepBoundary::Finished);
}

/// Test 4 — Calling `finish` before reaching `Finished`
/// produces a partial result reflecting the state so far.
/// Using the pausing provider, we drive one step that gets
/// aborted by cancellation; the partial assistant has
/// `StopReason::Aborted`.
///
/// `AgentRunMachine` borrows `&AgentLoop`, so we construct
/// the loop *inside* the spawned task — the borrow is then
/// bounded by the closure's body and the future is `'static`.
#[tokio::test]
async fn step_machine_finish_called_early_returns_partial_result() {
    let (tx, mut rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    let tx_for_task = tx.clone();

    let handle = tokio::spawn(async move {
        let agent_loop = build_loop_with_provider(Box::new(PausingProvider));
        let mut machine = agent_loop
            .start_run_machine(vec![user_prompt("pause")], Vec::new(), &tx_for_task)
            .await;
        machine.next_step(&tx_for_task, &cancel_for_task).await;
        machine.finish(&tx_for_task).await
    });
    drop(tx); // close the test-side sender so rx.recv eventually drains.

    // Drain until we see a TextDelta — the provider has
    // yielded its first event and is now parked. Cancel.
    while let Some(event) = rx.recv().await {
        if matches!(
            event,
            AgentEvent::MessageDelta {
                delta: StreamDelta::TextDelta(_)
            }
        ) {
            cancel.cancel();
            break;
        }
    }
    let result = handle.await.expect("step task panicked");

    assert!(result.terminal_error.is_none());
    let last_assistant = result
        .generated_messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) => Some(a.clone()),
            _ => None,
        })
        .expect("partial assistant present");
    assert_eq!(last_assistant.stop_reason, StopReason::Aborted);
}

/// Test 5 — Calling `next_step` with an already-cancelled token
/// converges quickly and finishes; subsequent `next_step` is a
/// no-op.
#[tokio::test]
async fn step_machine_passes_through_cancellation_per_step() {
    let scripts = vec![MockStreamScript::from_message(assistant_text(
        "ok",
        StopReason::Stop,
    ))];
    let agent_loop = build_loop_with_scripts(scripts);
    let (tx, _rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut machine = agent_loop
        .start_run_machine(vec![user_prompt("p")], Vec::new(), &tx)
        .await;

    // First step runs the model turn under a cancelled token.
    // The mock provider yields `Done` immediately, so the
    // assistant comes back as Stop (not Aborted) — the cancel
    // race only matters mid-stream. But the run still finishes
    // promptly via the no-tools-no-follow-ups branch.
    let _ = machine.next_step(&tx, &cancel).await;
    assert!(machine.is_finished());
    let boundary = machine.next_step(&tx, &cancel).await;
    assert_eq!(boundary, AgentStepBoundary::Finished);
}
