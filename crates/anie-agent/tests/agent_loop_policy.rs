//! Tests for the `BeforeModelPolicy` extension hook.
//!
//! Plan: `docs/repl_agent_loop/07_first_policy_boundary.md`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use anie_agent::{
    AgentLoop, AgentLoopConfig, BeforeModelPolicy, BeforeModelRequest, BeforeModelResponse,
    NoopBeforeModelPolicy, ToolExecutionMode, ToolRegistry,
};
use anie_protocol::{AssistantMessage, ContentBlock, Message, StopReason, Usage, UserMessage};
use anie_provider::{
    ApiKind, CostPerMillion, Model, ModelCompat, ProviderError, ProviderRegistry,
    RequestOptionsResolver, ResolvedRequestOptions, ThinkingLevel,
    mock::{MockProvider, MockStreamScript},
};
use async_trait::async_trait;
use tokio::sync::mpsc;
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

fn config_with_policy(policy: Option<Arc<dyn BeforeModelPolicy>>) -> AgentLoopConfig {
    let mut config = AgentLoopConfig::new(
        sample_model(),
        "system".into(),
        ThinkingLevel::Off,
        ToolExecutionMode::Sequential,
        Arc::new(StaticResolver),
    );
    if let Some(policy) = policy {
        config = config.with_before_model_policy(policy);
    }
    config
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

fn build_loop(scripts: Vec<MockStreamScript>, config: AgentLoopConfig) -> AgentLoop {
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(
        ApiKind::OpenAICompletions,
        Box::new(MockProvider::new(scripts)),
    );
    AgentLoop::new(
        Arc::new(provider_registry),
        Arc::new(ToolRegistry::new()),
        config,
    )
}

// =========================================================================
// Tests
// =========================================================================

/// `NoopBeforeModelPolicy` is the default; setting it
/// explicitly produces the same `AgentRunResult` as not setting
/// any policy. Confirms the hook integration didn't change
/// behavior for callers that opt out.
#[tokio::test]
async fn noop_policy_preserves_existing_behavior() {
    // Run 1: default (no explicit policy install — relies on
    // AgentLoopConfig::new's NoopBeforeModelPolicy default).
    let scripts1 = vec![MockStreamScript::from_message(assistant_text(
        "ok",
        StopReason::Stop,
    ))];
    let agent_loop1 = build_loop(scripts1, config_with_policy(None));
    let (tx1, _rx1) = mpsc::channel(64);
    let result1 = agent_loop1
        .run(
            vec![user_prompt("hi")],
            Vec::new(),
            tx1,
            CancellationToken::new(),
        )
        .await;

    // Run 2: explicit NoopBeforeModelPolicy install.
    let scripts2 = vec![MockStreamScript::from_message(assistant_text(
        "ok",
        StopReason::Stop,
    ))];
    let agent_loop2 = build_loop(
        scripts2,
        config_with_policy(Some(Arc::new(NoopBeforeModelPolicy))),
    );
    let (tx2, _rx2) = mpsc::channel(64);
    let result2 = agent_loop2
        .run(
            vec![user_prompt("hi")],
            Vec::new(),
            tx2,
            CancellationToken::new(),
        )
        .await;

    assert_eq!(result1.generated_messages, result2.generated_messages);
    assert_eq!(result1.final_context, result2.final_context);
    assert_eq!(result1.terminal_error, result2.terminal_error);
}

/// A policy that injects a context message on its first call
/// and returns Continue thereafter. Verifies:
/// 1. The injected message lands in `final_context` *before*
///    the assistant's reply.
/// 2. The injected message does *not* appear in
///    `generated_messages` (policy injections are not persisted
///    as agent output).
/// 3. The hook receives the correct `step_index` (zero on the
///    first call, monotonically increasing thereafter).
#[tokio::test]
async fn append_policy_injects_messages_into_context() {
    struct AppendOncePolicy {
        msg: Message,
        seen_step_indices: Mutex<Vec<u64>>,
        first_call_done: Mutex<bool>,
    }

    #[async_trait]
    impl BeforeModelPolicy for AppendOncePolicy {
        async fn before_model(&self, request: BeforeModelRequest<'_>) -> BeforeModelResponse {
            self.seen_step_indices
                .lock()
                .unwrap()
                .push(request.step_index);
            let mut first = self.first_call_done.lock().unwrap();
            if !*first {
                *first = true;
                BeforeModelResponse::AppendMessages(vec![self.msg.clone()])
            } else {
                BeforeModelResponse::Continue
            }
        }
    }

    let injected = Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: "policy injection".into(),
        }],
        timestamp: 99,
    });
    let policy = Arc::new(AppendOncePolicy {
        msg: injected.clone(),
        seen_step_indices: Mutex::new(Vec::new()),
        first_call_done: Mutex::new(false),
    });

    let scripts = vec![MockStreamScript::from_message(assistant_text(
        "after-injection",
        StopReason::Stop,
    ))];
    let agent_loop = build_loop(scripts, config_with_policy(Some(policy.clone())));
    let (tx, _rx) = mpsc::channel(64);
    let result = agent_loop
        .run(
            vec![user_prompt("hi")],
            Vec::new(),
            tx,
            CancellationToken::new(),
        )
        .await;

    // Final context: prompt → injected → assistant.
    assert_eq!(result.final_context.len(), 3);
    assert!(matches!(result.final_context[0], Message::User(_)));
    let injected_in_context = match &result.final_context[1] {
        Message::User(u) => match &u.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        },
        other => panic!("expected User, got {other:?}"),
    };
    assert_eq!(injected_in_context, "policy injection");
    assert!(matches!(result.final_context[2], Message::Assistant(_)));

    // Generated messages: only the assistant. Injection excluded.
    assert_eq!(result.generated_messages.len(), 1);
    assert!(matches!(
        result.generated_messages[0],
        Message::Assistant(_)
    ));

    // Step indices recorded by the hook: starts at 0,
    // monotonic. (One model turn here, so just [0].)
    let seen = policy.seen_step_indices.lock().unwrap().clone();
    assert_eq!(seen, vec![0u64]);
}

/// A policy that returns `ReplaceMessages` on its first call.
/// Verifies that the loop swaps the entire run context to the
/// supplied vector before sending the request to the provider.
/// This is the dispatch path
/// `BeforeModelResponse::ReplaceMessages` ->
/// `AgentRunState::replace_context`. Plan
/// `docs/rlm_2026-04-29/06_phased_implementation.md` Phase C.
#[tokio::test]
async fn replace_policy_swaps_run_context() {
    struct ReplaceOncePolicy {
        replacement: Vec<Message>,
        first_call_done: Mutex<bool>,
    }

    #[async_trait]
    impl BeforeModelPolicy for ReplaceOncePolicy {
        async fn before_model(&self, _request: BeforeModelRequest<'_>) -> BeforeModelResponse {
            let mut first = self.first_call_done.lock().unwrap();
            if !*first {
                *first = true;
                BeforeModelResponse::ReplaceMessages(self.replacement.clone())
            } else {
                BeforeModelResponse::Continue
            }
        }
    }

    let survivor = Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: "lone survivor".into(),
        }],
        timestamp: 7,
    });
    let policy = Arc::new(ReplaceOncePolicy {
        replacement: vec![survivor.clone()],
        first_call_done: Mutex::new(false),
    });

    let scripts = vec![MockStreamScript::from_message(assistant_text(
        "after-replace",
        StopReason::Stop,
    ))];
    let agent_loop = build_loop(scripts, config_with_policy(Some(policy)));
    let (tx, _rx) = mpsc::channel(64);
    let result = agent_loop
        .run(
            vec![user_prompt("would-be-evicted")],
            Vec::new(),
            tx,
            CancellationToken::new(),
        )
        .await;

    // Final context: replacement (1) + assistant reply (1).
    // The original prompt is gone — replaced wholesale.
    assert_eq!(result.final_context.len(), 2);
    let survivor_text = match &result.final_context[0] {
        Message::User(u) => match &u.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        },
        other => panic!("expected User, got {other:?}"),
    };
    assert_eq!(survivor_text, "lone survivor");
    assert!(matches!(result.final_context[1], Message::Assistant(_)));

    // The original prompt does *not* appear in final_context.
    let any_would_be_evicted = result.final_context.iter().any(|m| match m {
        Message::User(u) => match &u.content[0] {
            ContentBlock::Text { text } => text == "would-be-evicted",
            _ => false,
        },
        _ => false,
    });
    assert!(
        !any_would_be_evicted,
        "ReplaceMessages should drop the original prompt"
    );

    // generated_messages still contains only the assistant
    // reply — replacement happens on `context`, not on the
    // controller-persisted output stream.
    assert_eq!(result.generated_messages.len(), 1);
    assert!(matches!(
        result.generated_messages[0],
        Message::Assistant(_)
    ));
}
