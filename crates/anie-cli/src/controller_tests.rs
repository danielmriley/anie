use std::{fs, path::Path};

use super::*;
use crate::bootstrap::build_tool_registry;
use crate::runtime_state::RuntimeState;
use anie_protocol::StopReason;
use anie_provider::{
    ApiKind, CostPerMillion, ProviderError,
    mock::{MockProvider, MockStreamScript},
};
use anie_session::SessionManager;
use tempfile::tempdir;

fn model(id: &str, provider: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        provider: provider.into(),
        api: ApiKind::OpenAICompletions,
        base_url: "http://localhost:11434/v1".into(),
        context_window: 32_768,
        max_tokens: 8_192,
        supports_reasoning: false,
        reasoning_capabilities: None,
        supports_images: false,
        cost_per_million: CostPerMillion::zero(),
        replay_capabilities: None,
    }
}

fn assistant_message(text: &str) -> anie_protocol::AssistantMessage {
    anie_protocol::AssistantMessage {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        usage: anie_protocol::Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        provider: "openai".into(),
        model: "gpt-4o".into(),
        timestamp: 1,
    }
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        timestamp: 1,
    })
}

fn preload_compactable_history(session: &mut SessionManager) {
    for index in 0..4 {
        let text = format!("history-{index}-{}", "x".repeat(4_000));
        session
            .append_message(&user_message(&text))
            .expect("append history message");
    }
}

async fn run_prompt_with_provider_scripts(scripts: Vec<MockStreamScript>) -> Vec<AgentEvent> {
    let tempdir = tempdir().expect("tempdir");
    let cwd = tempdir.path().join("cwd");
    let sessions_dir = tempdir.path().join("sessions");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(&sessions_dir).expect("create sessions dir");

    let mut config = AnieConfig::default();
    config.compaction.enabled = false;
    config.compaction.keep_recent_tokens = 2_000;
    let tool_registry = build_tool_registry(&cwd, true);
    let prompt_cache =
        SystemPromptCache::build(&cwd, &tool_registry, &config).expect("prompt cache");

    let mut session = SessionManager::new_session(&sessions_dir, &cwd).expect("new session");
    preload_compactable_history(&mut session);

    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(
        ApiKind::OpenAICompletions,
        Box::new(MockProvider::new(scripts)),
    );

    let state = ControllerState {
        config: ConfigState::new(
            config.clone(),
            RuntimeState::default(),
            model("gpt-4o", "openai"),
            ThinkingLevel::Medium,
            None,
        ),
        session: SessionHandle::from_manager(session, sessions_dir, cwd.clone()),
        model_catalog: vec![model("gpt-4o", "openai")],
        provider_registry: Arc::new(provider_registry),
        tool_registry,
        request_options_resolver: Arc::new(AuthResolver::new(None, config)),
        prompt_cache,
        retry_config: RetryConfig {
            initial_delay_ms: 1,
            max_delay_ms: 1,
            backoff_multiplier: 1.0,
            max_retries: 3,
            jitter: false,
        },
        command_registry: crate::commands::CommandRegistry::with_builtins(),
    };

    let (event_tx, mut event_rx) = mpsc::channel(128);
    let (ui_action_tx, ui_action_rx) = mpsc::channel(8);
    let controller = InteractiveController::new(state, ui_action_rx, event_tx, true);
    let controller_task = tokio::spawn(async move { controller.run().await });

    ui_action_tx
        .send(UiAction::SubmitPrompt("retry me".into()))
        .await
        .expect("submit prompt");

    controller_task
        .await
        .expect("controller task join")
        .expect("controller run");

    let mut events = Vec::new();
    while let Some(event) = event_rx.recv().await {
        events.push(event);
    }
    events
}

fn agent_end_contains_text(events: &[AgentEvent], needle: &str) -> bool {
    events.iter().any(|event| {
        matches!(event,
            AgentEvent::AgentEnd { messages } if messages.iter().any(|message| {
                matches!(message,
                    Message::Assistant(assistant) if assistant.content.iter().any(|block| {
                        matches!(block, ContentBlock::Text { text } if text.contains(needle))
                    })
                )
            })
        )
    })
}

#[test]
fn no_tools_flag_builds_empty_registry() {
    let registry = build_tool_registry(Path::new("."), true);
    assert!(registry.definitions().is_empty());
}

#[test]
fn tool_registry_contains_core_tools_by_default() {
    let registry = build_tool_registry(Path::new("."), false);
    let names = registry
        .definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"read".to_string()));
    assert!(names.contains(&"write".to_string()));
    assert!(names.contains(&"edit".to_string()));
    assert!(names.contains(&"bash".to_string()));
}

#[test]
fn parse_thinking_accepts_supported_levels() {
    assert_eq!(
        parse_thinking_level("off").expect("off"),
        ThinkingLevel::Off
    );
    assert_eq!(
        parse_thinking_level("low").expect("low"),
        ThinkingLevel::Low
    );
    assert_eq!(
        parse_thinking_level("medium").expect("medium"),
        ThinkingLevel::Medium
    );
    assert_eq!(
        parse_thinking_level("high").expect("high"),
        ThinkingLevel::High
    );
}

#[tokio::test]
async fn controller_compaction_retry_path() {
    let events = run_prompt_with_provider_scripts(vec![
        MockStreamScript::from_error(ProviderError::ContextOverflow("too many tokens".into())),
        MockStreamScript::from_message(assistant_message("compaction summary")),
        MockStreamScript::from_message(assistant_message("recovered after compaction")),
    ])
    .await;

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::CompactionStart))
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::CompactionEnd { .. }))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::AgentStart))
            .count(),
        2
    );
    assert!(agent_end_contains_text(
        &events,
        "recovered after compaction"
    ));
}

#[tokio::test]
async fn controller_compaction_give_up_after_second_overflow() {
    let events = run_prompt_with_provider_scripts(vec![
        MockStreamScript::from_error(ProviderError::ContextOverflow("too many tokens".into())),
        MockStreamScript::from_message(assistant_message("compaction summary")),
        MockStreamScript::from_error(ProviderError::ContextOverflow(
            "still too many tokens".into(),
        )),
    ])
    .await;

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::CompactionStart))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::AgentStart))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::RetryScheduled { .. }))
            .count(),
        0
    );
    assert!(agent_end_contains_text(&events, "Context overflow"));
}

#[tokio::test]
async fn controller_transient_retry_exhausts_attempts() {
    let events = run_prompt_with_provider_scripts(vec![
        MockStreamScript::from_error(ProviderError::Transport("dns".into())),
        MockStreamScript::from_error(ProviderError::Transport("dns".into())),
        MockStreamScript::from_error(ProviderError::Transport("dns".into())),
        MockStreamScript::from_error(ProviderError::Transport("dns".into())),
    ])
    .await;

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::RetryScheduled { .. }))
            .count(),
        3
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::AgentStart))
            .count(),
        4
    );
    assert!(agent_end_contains_text(&events, "Transport error: dns"));
}

#[tokio::test]
async fn help_command_emits_system_message_with_registry_output() {
    let tempdir = tempdir().expect("tempdir");
    let cwd = tempdir.path().join("cwd");
    let sessions_dir = tempdir.path().join("sessions");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(&sessions_dir).expect("create sessions dir");

    let config = AnieConfig::default();
    let tool_registry = build_tool_registry(&cwd, true);
    let prompt_cache =
        SystemPromptCache::build(&cwd, &tool_registry, &config).expect("build prompt cache");
    let session = SessionManager::new_session(&sessions_dir, &cwd).expect("new session");
    let command_registry = crate::commands::CommandRegistry::with_builtins();
    let expected = command_registry.format_help();

    let state = ControllerState {
        config: ConfigState::new(
            config.clone(),
            RuntimeState::default(),
            model("gpt-4o", "openai"),
            ThinkingLevel::Medium,
            None,
        ),
        session: SessionHandle::from_manager(session, sessions_dir, cwd.clone()),
        model_catalog: vec![model("gpt-4o", "openai")],
        provider_registry: Arc::new(ProviderRegistry::new()),
        tool_registry,
        request_options_resolver: Arc::new(AuthResolver::new(None, config)),
        prompt_cache,
        retry_config: RetryConfig::default(),
        command_registry,
    };

    let (_ui_action_tx, ui_action_rx) = mpsc::channel(1);
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let mut controller = InteractiveController::new(state, ui_action_rx, event_tx, false);

    controller
        .handle_action(UiAction::ShowHelp)
        .await
        .expect("handle help action");

    let event = event_rx.recv().await.expect("help event");
    assert!(matches!(
        event,
        AgentEvent::SystemMessage { text } if text == expected
    ));
}
