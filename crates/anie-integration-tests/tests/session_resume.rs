use std::sync::{Arc, Mutex};

use anie_integration_tests::helpers::*;
use anie_protocol::{
    AgentEvent, AssistantMessage, ContentBlock, Message, StopReason, ToolDef, Usage, now_millis,
};
use anie_provider::mock::MockStreamScript;
use anie_provider::{
    ApiKind, LlmContext, LlmMessage, Model, Provider, ProviderError, ProviderEvent,
    ProviderRegistry, ProviderStream, StreamOptions, ThinkingLevel,
};
use anie_session::{EntryBase, SessionEntry, SessionManager};

use anie_agent::{AgentLoop, AgentLoopConfig, ToolExecutionMode, ToolRegistry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn resumed_session_context_drives_new_agent_run() {
    let (dir, mut session) = create_temp_session();
    let _ = dir; // keep alive

    let prompt = user_prompt("Read the file.");
    session.append_message(&prompt).expect("persist prompt");
    session
        .append_messages(&[Message::Assistant(final_assistant(
            "I read the file. It contains hello.",
        ))])
        .expect("persist assistant");

    let session_path = session.path().to_path_buf();
    drop(session);

    let reopened = SessionManager::open_session(&session_path).expect("reopen");
    let prior_context: Vec<Message> = reopened
        .build_context()
        .messages
        .into_iter()
        .map(|m| m.message)
        .collect();
    assert_eq!(prior_context.len(), 2);

    let provider = anie_provider::mock::MockProvider::new(vec![MockStreamScript::from_message(
        final_assistant("Yes, I remember the file."),
    )]);
    let agent = build_agent(
        provider,
        std::sync::Arc::new(anie_agent::ToolRegistry::new()),
    );
    let new_prompt = user_prompt("Do you remember what the file contained?");
    let (result, _events) =
        run_agent_collecting_events(agent, vec![new_prompt], prior_context).await;

    assert!(result.terminal_error.is_none());
    assert_eq!(result.generated_messages.len(), 1);
    assert_eq!(result.final_context.len(), 4);
}

#[tokio::test]
async fn thinking_blocks_survive_session_roundtrip_into_agent_context() {
    let (dir, mut session) = create_temp_session();
    let _ = dir;

    let prompt = user_prompt("Think about this.");
    session.append_message(&prompt).expect("persist prompt");

    let assistant = Message::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Thinking {
                thinking: "Let me reason about this.".into(),
                signature: None,
            },
            ContentBlock::Text {
                text: "The answer is 42.".into(),
            },
        ],
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 1,
    });
    session
        .append_messages(&[assistant])
        .expect("persist assistant");

    let session_path = session.path().to_path_buf();
    drop(session);

    let reopened = SessionManager::open_session(&session_path).expect("reopen");
    let context = reopened.build_context();
    assert_eq!(context.messages.len(), 2);

    let assistant_msg = match &context.messages[1].message {
        Message::Assistant(a) => a,
        other => panic!("expected Assistant, got {other:?}"),
    };
    assert_eq!(assistant_msg.content.len(), 2);
    assert!(matches!(
        &assistant_msg.content[0],
        ContentBlock::Thinking { thinking, .. } if thinking == "Let me reason about this."
    ));
    assert!(matches!(
        &assistant_msg.content[1],
        ContentBlock::Text { text } if text == "The answer is 42."
    ));
}

#[tokio::test]
async fn compacted_session_context_drives_agent_run() {
    let (dir, mut session) = create_temp_session();
    let _ = dir;

    // Persist 4 question/answer exchanges with incrementing timestamps.
    let exchanges = 4;
    let mut entry_ids = Vec::new();
    for i in 0..exchanges {
        let ts = (i as u64 + 1) * 100;
        let id = session
            .append_message(&user_prompt_at(&format!("question {i}"), ts))
            .expect("persist prompt");
        entry_ids.push(id);
        let ids = session
            .append_messages(&[Message::Assistant(final_assistant_at(
                &format!("answer {i}"),
                ts + 50,
            ))])
            .expect("persist assistant");
        entry_ids.extend(ids);
    }

    // Each exchange produces 2 entry IDs (question + answer).  We want to
    // keep from the last exchange onward (exchange index 3, i.e. "question 3").
    let kept_exchange = 3;
    let first_kept_id = entry_ids[kept_exchange * 2].clone();
    session
        .add_entries(vec![SessionEntry::Compaction {
            base: EntryBase {
                id: format!("compact-{}", entry_ids.len()),
                parent_id: session.leaf_id().map(str::to_string),
                timestamp: "500".into(),
            },
            summary: "Prior discussion covered questions 0 through 2.".into(),
            first_kept_entry_id: first_kept_id,
            tokens_before: 5000,
        }])
        .expect("add compaction");

    let session_path = session.path().to_path_buf();
    drop(session);

    let reopened = SessionManager::open_session(&session_path).expect("reopen");
    let context = reopened.build_context();

    // Context should have: compaction summary + remaining exchange (question 3 + answer 3).
    assert!(
        context.messages.len() <= 4,
        "expected compacted context, got {} messages",
        context.messages.len()
    );
    // The first message should be the compaction summary (a user-shaped message).
    let first_text = match &context.messages[0].message {
        Message::User(u) => u
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or(""),
        Message::Assistant(a) => a
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or(""),
        _ => "",
    };
    assert!(
        first_text.contains("Prior discussion"),
        "first message was: {first_text}"
    );

    // Drive a new agent run with the compacted context.
    let prior: Vec<Message> = context.messages.into_iter().map(|m| m.message).collect();
    let provider = anie_provider::mock::MockProvider::new(vec![MockStreamScript::from_message(
        final_assistant("Continuing from compacted context."),
    )]);
    let agent = build_agent(
        provider,
        std::sync::Arc::new(anie_agent::ToolRegistry::new()),
    );
    let (result, _events) =
        run_agent_collecting_events(agent, vec![user_prompt("Continue.")], prior).await;

    assert!(result.terminal_error.is_none());
    assert_eq!(result.generated_messages.len(), 1);
}

/// A provider that:
/// - declares `requires_thinking_signature == true` (simulates Anthropic),
/// - records every `Vec<Message>` passed to `convert_messages`,
/// - returns a canned final assistant message on `stream()`.
///
/// The agent loop calls `sanitize_context_for_request` before
/// `convert_messages`, so the captured messages reflect the post-
/// sanitizer state. If the sanitizer fails to drop an unsigned
/// thinking block, this spy will record it — and the assertions
/// below will fail.
struct SignatureRequiringSpy {
    captured: Arc<Mutex<Vec<Vec<Message>>>>,
    response: AssistantMessage,
}

impl SignatureRequiringSpy {
    fn new(response: AssistantMessage) -> (Self, Arc<Mutex<Vec<Vec<Message>>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                captured: Arc::clone(&captured),
                response,
            },
            captured,
        )
    }
}

impl Provider for SignatureRequiringSpy {
    fn stream(
        &self,
        _model: &Model,
        _context: LlmContext,
        _options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        let response = self.response.clone();
        Ok(Box::pin(futures::stream::once(async move {
            Ok(ProviderEvent::Done(response))
        })))
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
        self.captured.lock().unwrap().push(messages.to_vec());
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
                content: serde_json::Value::Null,
            })
            .collect()
    }

    fn includes_thinking_in_replay(&self) -> bool {
        true
    }

    fn convert_tools(&self, _tools: &[ToolDef]) -> Vec<serde_json::Value> {
        Vec::new()
    }
}

#[tokio::test]
async fn legacy_unsigned_thinking_is_dropped_before_replay() {
    // Write a "legacy" session: an assistant turn with a thinking
    // block that has no signature, followed by visible text. This
    // mirrors a session persisted before plan 01a landed.
    let (dir, mut session) = create_temp_session();
    let _ = dir;

    session
        .append_message(&user_prompt("initial question"))
        .expect("persist prompt");
    session
        .append_messages(&[Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "legacy reasoning".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "legacy answer".into(),
                },
            ],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
            timestamp: 1,
        })])
        .expect("persist assistant");

    let session_path = session.path().to_path_buf();
    drop(session);

    let reopened = SessionManager::open_session(&session_path).expect("reopen");
    let prior_context: Vec<Message> = reopened
        .build_context()
        .messages
        .into_iter()
        .map(|m| m.message)
        .collect();

    // Sanity: the reopened unsigned thinking block survived the roundtrip.
    let has_unsigned = prior_context.iter().any(|m| {
        if let Message::Assistant(a) = m {
            a.content.iter().any(|b| {
                matches!(
                    b,
                    ContentBlock::Thinking {
                        signature: None,
                        ..
                    }
                )
            })
        } else {
            false
        }
    });
    assert!(has_unsigned, "legacy unsigned thinking must survive load");

    // Build an agent whose provider requires signatures and captures
    // the messages it is asked to convert.
    let response = AssistantMessage {
        content: vec![ContentBlock::Text {
            text: "fresh reply".into(),
        }],
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        provider: "spy".into(),
        model: "claude-sonnet-4-6".into(),
        timestamp: now_millis(),
    };
    let (spy, captured) = SignatureRequiringSpy::new(response);

    let mut registry = ProviderRegistry::new();
    registry.register(ApiKind::AnthropicMessages, Box::new(spy));

    let anthropic_model = Model {
        id: "claude-sonnet-4-6".into(),
        name: "Claude".into(),
        provider: "anthropic".into(),
        api: ApiKind::AnthropicMessages,
        base_url: "https://api.anthropic.com".into(),
        context_window: 200_000,
        max_tokens: 8_192,
        supports_reasoning: true,
        reasoning_capabilities: None,
        supports_images: true,
        cost_per_million: anie_provider::CostPerMillion::zero(),
        // Declare signature requirement on the model — the sanitizer
        // reads the flag from here now (plan 03c moved it off the
        // Provider trait).
        replay_capabilities: Some(anie_provider::ReplayCapabilities {
            requires_thinking_signature: true,
            supports_redacted_thinking: true,
            supports_encrypted_reasoning: false,
        }),
        compat: anie_provider::ModelCompat::None,
    };
    let agent = AgentLoop::new(
        Arc::new(registry),
        Arc::new(ToolRegistry::new()),
        AgentLoopConfig::new(
            anthropic_model,
            "Test agent.".into(),
            ThinkingLevel::Off,
            ToolExecutionMode::Parallel,
            static_resolver(),
        ),
    );

    // Send a follow-up user turn. The agent's sanitizer will run
    // before convert_messages; the spy records what it sees.
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(128);
    let cancel = CancellationToken::new();
    let handle = tokio::spawn(async move {
        agent
            .run(
                vec![user_prompt("follow up")],
                prior_context,
                event_tx,
                cancel,
            )
            .await
    });
    while let Some(event) = event_rx.recv().await {
        if matches!(event, AgentEvent::AgentEnd { .. }) {
            break;
        }
    }
    let result = handle.await.expect("agent task");
    assert!(result.terminal_error.is_none());

    // The spy's captured first-call messages should contain NO
    // thinking block. The sanitizer dropped the unsigned one before
    // it reached the provider's wire-format conversion.
    let calls = captured.lock().unwrap();
    assert!(
        !calls.is_empty(),
        "spy should have been called at least once"
    );
    for call in calls.iter() {
        for message in call {
            if let Message::Assistant(assistant) = message {
                let has_thinking = assistant
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Thinking { .. }));
                assert!(
                    !has_thinking,
                    "sanitizer must drop unsigned thinking blocks before replay; \
                     assistant still carried: {:?}",
                    assistant.content
                );
            }
        }
    }
}
