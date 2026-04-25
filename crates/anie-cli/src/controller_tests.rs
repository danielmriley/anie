use std::{fs, path::Path};

use super::*;
use crate::bootstrap::build_tool_registry;
use crate::runtime_state::RuntimeState;
use anie_protocol::StopReason;
use anie_provider::{
    ApiKind, CostPerMillion, ModelCompat, ProviderError,
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
        compat: ModelCompat::None,
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
        reasoning_details: None,
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
    let (ui_action_tx, ui_action_rx) = mpsc::unbounded_channel();
    let controller = InteractiveController::new(state, ui_action_rx, event_tx, true);
    let controller_task = tokio::spawn(async move { controller.run().await });

    ui_action_tx
        .send(UiAction::SubmitPrompt("retry me".into()))
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
    assert!(names.contains(&"grep".to_string()));
    assert!(names.contains(&"find".to_string()));
    assert!(names.contains(&"ls".to_string()));
}

#[test]
fn parse_thinking_accepts_supported_levels() {
    assert_eq!(
        parse_thinking_level("off").expect("off"),
        ThinkingLevel::Off
    );
    assert_eq!(
        parse_thinking_level("minimal").expect("minimal"),
        ThinkingLevel::Minimal
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

#[test]
fn thinking_levels_order_from_off_to_high() {
    // Variant ordering is observable via PartialOrd and is
    // documented in `anie-provider/src/thinking.rs`. The order
    // is Off < Minimal < Low < Medium < High.
    assert!(ThinkingLevel::Off < ThinkingLevel::Minimal);
    assert!(ThinkingLevel::Minimal < ThinkingLevel::Low);
    assert!(ThinkingLevel::Low < ThinkingLevel::Medium);
    assert!(ThinkingLevel::Medium < ThinkingLevel::High);
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

/// Build a minimal controller for dispatcher tests. Returns the
/// controller plus the event receiver so assertions can inspect
/// emitted events.
fn build_dispatch_controller(
    catalog: Vec<Model>,
    event_capacity: usize,
) -> (
    InteractiveController,
    mpsc::Receiver<AgentEvent>,
    mpsc::UnboundedSender<UiAction>,
) {
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
    let default_model = catalog
        .first()
        .cloned()
        .unwrap_or_else(|| model("gpt-4o", "openai"));

    let state = ControllerState {
        config: ConfigState::new(
            config.clone(),
            RuntimeState::default(),
            default_model.clone(),
            ThinkingLevel::Medium,
            None,
        ),
        session: SessionHandle::from_manager(session, sessions_dir, cwd),
        model_catalog: if catalog.is_empty() {
            vec![default_model]
        } else {
            catalog
        },
        provider_registry: Arc::new(ProviderRegistry::new()),
        tool_registry,
        request_options_resolver: Arc::new(AuthResolver::new(None, config)),
        prompt_cache,
        retry_config: RetryConfig::default(),
        command_registry: crate::commands::CommandRegistry::with_builtins(),
    };

    let (ui_action_tx, ui_action_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::channel(event_capacity);
    let controller = InteractiveController::new(state, ui_action_rx, event_tx, false);

    (controller, event_rx, ui_action_tx)
}

fn system_message_text(event: &AgentEvent) -> Option<&str> {
    match event {
        AgentEvent::SystemMessage { text } => Some(text.as_str()),
        _ => None,
    }
}

async fn drain_next_system_message(rx: &mut mpsc::Receiver<AgentEvent>) -> String {
    loop {
        let event = rx.recv().await.expect("event");
        if let Some(text) = system_message_text(&event) {
            return text.to_string();
        }
    }
}

#[tokio::test]
async fn invalid_thinking_level_emits_system_message() {
    let (mut controller, mut event_rx, _tx) = build_dispatch_controller(Vec::new(), 16);

    controller
        .handle_action(UiAction::SetThinking("bogus".into()))
        .await
        .expect("invalid thinking must not terminate controller");

    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(
        msg.contains("bogus") && msg.contains("off") && msg.contains("high"),
        "expected helpful error listing accepted levels, got: {msg}"
    );
}

#[tokio::test]
async fn invalid_thinking_level_does_not_terminate_controller() {
    let (mut controller, mut event_rx, _tx) = build_dispatch_controller(Vec::new(), 16);

    controller
        .handle_action(UiAction::SetThinking("bogus".into()))
        .await
        .expect("bad thinking action returns Ok");

    // First system message: the rejection.
    let first = drain_next_system_message(&mut event_rx).await;
    assert!(first.contains("bogus"), "first message should be the error");

    // Controller is still live — next action must fire.
    controller
        .handle_action(UiAction::GetState)
        .await
        .expect("subsequent action still dispatches");

    // Advance past the status update emitted by GetState, then
    // read the session/provider/model/thinking summary.
    let summary = drain_next_system_message(&mut event_rx).await;
    assert!(
        summary.contains("Thinking: medium"),
        "state summary missing: {summary}"
    );
}

#[tokio::test]
async fn valid_thinking_level_emits_success_message_and_updates_state() {
    let (mut controller, mut event_rx, _tx) = build_dispatch_controller(Vec::new(), 16);

    controller
        .handle_action(UiAction::SetThinking("high".into()))
        .await
        .expect("valid thinking succeeds");

    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(
        msg.contains("Thinking level set to high"),
        "expected success confirmation, got: {msg}"
    );
    assert_eq!(
        controller.state.config.current_thinking(),
        ThinkingLevel::High
    );
}

#[tokio::test]
async fn unknown_session_switch_is_reported_not_fatal() {
    let (mut controller, mut event_rx, _tx) = build_dispatch_controller(Vec::new(), 16);

    controller
        .handle_action(UiAction::SwitchSession("nope".into()))
        .await
        .expect("unknown session must not terminate controller");

    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(
        msg.contains("unknown session") && msg.contains("nope"),
        "expected UnknownSession message, got: {msg}"
    );

    // Controller still alive.
    controller
        .handle_action(UiAction::GetState)
        .await
        .expect("still dispatching after unknown session");
}

#[tokio::test]
async fn unknown_model_switch_is_reported_not_fatal() {
    let catalog = vec![model("gpt-4o", "openai")];
    let (mut controller, mut event_rx, _tx) = build_dispatch_controller(catalog, 16);

    controller
        .handle_action(UiAction::SetModel("gpt-nonexistent".into()))
        .await
        .expect("unknown model must not terminate controller");

    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(
        msg.contains("unknown model") && msg.contains("gpt-nonexistent"),
        "expected UnknownModel message, got: {msg}"
    );
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

    let (_ui_action_tx, ui_action_rx) = mpsc::unbounded_channel();
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

// =============================================================================
// Plan 13 phase A — non-blocking retry backoff.
//
// These tests drive a live controller via its ui_action channel
// so they can inject abort/quit/etc. during the backoff window.
// They use `tokio::time::pause` where needed so they don't
// actually wait on wall-clock delays.
// =============================================================================

/// Build a controller ready to accept user actions. Returns the
/// action sender, event receiver, and the `JoinHandle` for the
/// run task so the test can await shutdown.
fn spawn_live_controller(
    scripts: Vec<MockStreamScript>,
    retry_config: RetryConfig,
) -> (
    mpsc::UnboundedSender<UiAction>,
    mpsc::Receiver<AgentEvent>,
    tokio::task::JoinHandle<Result<()>>,
) {
    let tempdir = tempdir().expect("tempdir");
    let cwd = tempdir.path().join("cwd");
    let sessions_dir = tempdir.path().join("sessions");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(&sessions_dir).expect("create sessions dir");

    let mut config = AnieConfig::default();
    config.compaction.enabled = false;
    let tool_registry = build_tool_registry(&cwd, true);
    let prompt_cache =
        SystemPromptCache::build(&cwd, &tool_registry, &config).expect("prompt cache");

    let session = SessionManager::new_session(&sessions_dir, &cwd).expect("new session");

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
        session: SessionHandle::from_manager(session, sessions_dir, cwd),
        model_catalog: vec![model("gpt-4o", "openai"), model("gpt-4.1", "openai")],
        provider_registry: Arc::new(provider_registry),
        tool_registry,
        request_options_resolver: Arc::new(AuthResolver::new(None, config)),
        prompt_cache,
        retry_config,
        command_registry: crate::commands::CommandRegistry::with_builtins(),
    };

    let (event_tx, event_rx) = mpsc::channel(128);
    let (ui_action_tx, ui_action_rx) = mpsc::unbounded_channel();
    let controller = InteractiveController::new(state, ui_action_rx, event_tx, false);
    let handle = tokio::spawn(async move { controller.run().await });

    (ui_action_tx, event_rx, handle)
}

/// Drain events from `rx` until `predicate` returns Some. Returns
/// the matching event along with every event drained up to that
/// point (inclusive).
async fn wait_for_event<F>(
    rx: &mut mpsc::Receiver<AgentEvent>,
    mut predicate: F,
) -> (Vec<AgentEvent>, AgentEvent)
where
    F: FnMut(&AgentEvent) -> bool,
{
    let mut seen = Vec::new();
    loop {
        let event = rx.recv().await.expect("event");
        if predicate(&event) {
            seen.push(event.clone());
            return (seen, event);
        }
        seen.push(event);
    }
}

fn retry_config_for_tests(delay_ms: u64, max_retries: u32) -> RetryConfig {
    RetryConfig {
        initial_delay_ms: delay_ms,
        max_delay_ms: delay_ms,
        backoff_multiplier: 1.0,
        max_retries,
        jitter: false,
    }
}

#[tokio::test]
async fn retry_backoff_polls_ui_actions() {
    // Regression: during transient-retry backoff, the controller
    // used to block on `tokio::time::sleep` and ignore
    // `ui_action_rx`. Any user action submitted during that
    // window was invisible until the sleep returned. After the
    // fix, the main `select!` polls actions alongside a
    // `sleep_until(deadline)` branch.
    tokio::time::pause();

    let (ui_tx, mut event_rx, handle) = spawn_live_controller(
        vec![
            MockStreamScript::from_error(ProviderError::Transport("dns".into())),
            MockStreamScript::from_message(assistant_message("eventually ok")),
        ],
        retry_config_for_tests(60_000, 3),
    );

    ui_tx
        .send(UiAction::SubmitPrompt("go".into()))
        .expect("submit prompt");

    // Wait for retry to be armed.
    wait_for_event(&mut event_rx, |event| {
        matches!(event, AgentEvent::RetryScheduled { .. })
    })
    .await;

    // UI action during the backoff window arrives and is
    // processed promptly. If this test ever hangs, the
    // non-blocking backoff isn't in place.
    ui_tx.send(UiAction::GetState).expect("send GetState");
    wait_for_event(
        &mut event_rx,
        |event| matches!(event, AgentEvent::SystemMessage { text } if text.contains("Session:")),
    )
    .await;

    // Clean up: advance time past the retry deadline so the
    // continuation runs and the controller drains.
    tokio::time::advance(Duration::from_millis(60_001)).await;
    ui_tx.send(UiAction::Quit).expect("quit");
    drop(ui_tx);
    handle
        .await
        .expect("controller join")
        .expect("controller run");
}

#[tokio::test]
async fn abort_during_retry_backoff_cancels_retry() {
    // Regression: `UiAction::Abort` during backoff clears the
    // pending retry. No continuation run must spawn, even after
    // the deadline elapses.
    tokio::time::pause();

    let (ui_tx, mut event_rx, handle) = spawn_live_controller(
        vec![
            MockStreamScript::from_error(ProviderError::Transport("dns".into())),
            MockStreamScript::from_message(assistant_message("should never run")),
        ],
        retry_config_for_tests(60_000, 3),
    );

    ui_tx
        .send(UiAction::SubmitPrompt("go".into()))
        .expect("submit prompt");
    wait_for_event(&mut event_rx, |event| {
        matches!(event, AgentEvent::RetryScheduled { .. })
    })
    .await;

    ui_tx.send(UiAction::Abort).expect("abort");
    wait_for_event(&mut event_rx, |event| {
        matches!(event, AgentEvent::SystemMessage { text } if text == "Retry aborted by user.")
    })
    .await;

    // Advance past the original retry deadline — the continuation
    // must not fire because the pending_retry state was cleared.
    tokio::time::advance(Duration::from_millis(60_001)).await;

    // Close the action channel to let the controller exit.
    drop(ui_tx);
    handle
        .await
        .expect("controller join")
        .expect("controller run");

    // Assert only the single initial AgentStart was emitted —
    // no continuation run spawned.
    let remaining: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    let agent_starts = remaining
        .iter()
        .filter(|event| matches!(event, AgentEvent::AgentStart))
        .count();
    assert_eq!(
        agent_starts, 0,
        "no second AgentStart after abort; drained events: {remaining:#?}"
    );
}

async fn assert_run_affecting_action_cancels_armed_retry(action: UiAction) {
    tokio::time::pause();

    let (ui_tx, mut event_rx, handle) = spawn_live_controller(
        vec![
            MockStreamScript::from_error(ProviderError::Transport("dns".into())),
            MockStreamScript::from_message(assistant_message("should never run")),
        ],
        retry_config_for_tests(60_000, 3),
    );

    ui_tx
        .send(UiAction::SubmitPrompt("go".into()))
        .expect("submit prompt");
    wait_for_event(&mut event_rx, |event| {
        matches!(event, AgentEvent::RetryScheduled { .. })
    })
    .await;

    ui_tx.send(action).expect("send run setting change");
    wait_for_event(&mut event_rx, |event| {
        matches!(event, AgentEvent::SystemMessage { text }
            if text == "Pending retry canceled because run settings changed.")
    })
    .await;

    tokio::time::advance(Duration::from_millis(60_001)).await;
    ui_tx.send(UiAction::Quit).expect("quit");
    drop(ui_tx);
    handle
        .await
        .expect("controller join")
        .expect("controller run");

    let remaining: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    assert!(
        !remaining
            .iter()
            .any(|event| matches!(event, AgentEvent::AgentStart)),
        "no continuation AgentStart after run setting change; drained events: {remaining:#?}"
    );
}

#[tokio::test]
async fn set_model_during_retry_backoff_cancels_retry() {
    assert_run_affecting_action_cancels_armed_retry(UiAction::SetModel("gpt-4.1".into())).await;
}

#[tokio::test]
async fn set_resolved_model_during_retry_backoff_cancels_retry() {
    assert_run_affecting_action_cancels_armed_retry(UiAction::SetResolvedModel(Box::new(model(
        "gpt-4.1", "openai",
    ))))
    .await;
}

#[tokio::test]
async fn set_thinking_during_retry_backoff_cancels_retry() {
    assert_run_affecting_action_cancels_armed_retry(UiAction::SetThinking("high".into())).await;
}

#[tokio::test]
async fn compact_during_retry_backoff_cancels_retry() {
    assert_run_affecting_action_cancels_armed_retry(UiAction::Compact).await;
}

#[tokio::test]
async fn new_session_during_retry_backoff_cancels_retry() {
    assert_run_affecting_action_cancels_armed_retry(UiAction::NewSession).await;
}

#[tokio::test]
async fn fork_session_during_retry_backoff_cancels_retry() {
    assert_run_affecting_action_cancels_armed_retry(UiAction::ForkSession).await;
}

#[tokio::test]
async fn quit_during_retry_backoff_exits_cleanly() {
    // Regression: `UiAction::Quit` during backoff exits the
    // controller without waiting for the deadline and without
    // spawning a continuation.
    tokio::time::pause();

    let (ui_tx, mut event_rx, handle) = spawn_live_controller(
        vec![
            MockStreamScript::from_error(ProviderError::Transport("dns".into())),
            MockStreamScript::from_message(assistant_message("should never run")),
        ],
        retry_config_for_tests(60_000, 3),
    );

    ui_tx
        .send(UiAction::SubmitPrompt("go".into()))
        .expect("submit prompt");
    wait_for_event(&mut event_rx, |event| {
        matches!(event, AgentEvent::RetryScheduled { .. })
    })
    .await;

    ui_tx.send(UiAction::Quit).expect("quit");

    // Controller must terminate without needing the deadline.
    handle
        .await
        .expect("controller join")
        .expect("controller run");
}

#[tokio::test]
async fn retry_fires_continuation_when_deadline_elapses() {
    // Happy path: without any intervening user action, the
    // deadline fires and the continuation run starts. This pins
    // that the refactor didn't break the normal retry path.
    tokio::time::pause();

    let (ui_tx, mut event_rx, handle) = spawn_live_controller(
        vec![
            MockStreamScript::from_error(ProviderError::Transport("dns".into())),
            MockStreamScript::from_message(assistant_message("recovered")),
        ],
        retry_config_for_tests(60_000, 3),
    );

    ui_tx
        .send(UiAction::SubmitPrompt("go".into()))
        .expect("submit prompt");
    wait_for_event(&mut event_rx, |event| {
        matches!(event, AgentEvent::RetryScheduled { .. })
    })
    .await;

    // Advance past the deadline — the second run should start.
    tokio::time::advance(Duration::from_millis(60_001)).await;
    wait_for_event(&mut event_rx, |event| {
        matches!(event, AgentEvent::AgentEnd { messages }
            if messages.iter().any(|m| matches!(m,
                Message::Assistant(a) if a.content.iter().any(|b|
                    matches!(b, ContentBlock::Text { text } if text == "recovered")))))
    })
    .await;

    ui_tx.send(UiAction::Quit).expect("quit");
    handle
        .await
        .expect("controller join")
        .expect("controller run");
}

// =============================================================================
// Plan 13 phase B — unbounded UiAction channel.
//
// Previously `action_tx` was a bounded `mpsc::Sender<UiAction>`
// sized at 64, and every call site used `try_send` with the
// `Err` dropped. Combined with the (now-fixed) blocking retry
// backoff in Phase A, a user could submit actions into a full
// channel and have them silently discarded. After the switch to
// `unbounded_channel`, `send` is synchronous and can only fail
// if the receiver has been dropped — the "channel is full" error
// no longer exists.
// =============================================================================

#[tokio::test]
async fn unbounded_action_channel_accepts_burst_without_drops() {
    // Enqueue a large number of idempotent actions back-to-back.
    // With the previous bounded channel (capacity 64) only the
    // first 64 would have been accepted. With the unbounded
    // channel every send succeeds and the controller drains the
    // whole burst.
    let (ui_tx, mut event_rx, handle) = spawn_live_controller(
        vec![MockStreamScript::from_message(assistant_message("ok"))],
        retry_config_for_tests(1, 0),
    );

    const BURST: usize = 1_000;
    for _ in 0..BURST {
        ui_tx
            .send(UiAction::GetState)
            .expect("unbounded send never fails while receiver is live");
    }

    // Drain `BURST` status responses. Each GetState emits one
    // StatusUpdate and one SystemMessage; we count the system
    // messages since they are the controller-side evidence that
    // the action was processed.
    let mut processed = 0;
    while processed < BURST {
        let event = event_rx.recv().await.expect("event");
        if matches!(event, AgentEvent::SystemMessage { ref text } if text.starts_with("Session:")) {
            processed += 1;
        }
    }
    assert_eq!(processed, BURST);

    ui_tx.send(UiAction::Quit).expect("quit");
    handle
        .await
        .expect("controller join")
        .expect("controller run");
}

#[tokio::test]
async fn send_to_closed_action_channel_errors_without_panic() {
    // When the controller has exited, the receiver is dropped
    // and any subsequent `send` returns `Err(SendError)`. The
    // failure must be non-panicking; TUI call sites that use
    // `let _ = action_tx.send(...)` rely on this.
    let (ui_tx, _event_rx, handle) = spawn_live_controller(
        vec![MockStreamScript::from_message(assistant_message("ok"))],
        retry_config_for_tests(1, 0),
    );

    // Send Quit, let the controller exit, then try to send
    // another action.
    ui_tx.send(UiAction::Quit).expect("quit");
    handle
        .await
        .expect("controller join")
        .expect("controller run");

    let result = ui_tx.send(UiAction::GetState);
    assert!(
        result.is_err(),
        "sending to a closed unbounded channel must return Err"
    );
}

#[tokio::test]
async fn unbounded_channel_preserves_fifo_order_under_burst() {
    // Regression: user actions must be processed in the order
    // they were submitted. Bounded `try_send` could drop
    // whichever action hit a full queue; unbounded send
    // preserves order deterministically.
    let (ui_tx, mut event_rx, handle) = spawn_live_controller(
        vec![MockStreamScript::from_message(assistant_message("ok"))],
        retry_config_for_tests(1, 0),
    );

    // Submit ShowHelp then GetState and verify the system
    // messages arrive in that order. Both are fast, idempotent,
    // and only ever emit a single SystemMessage so the ordering
    // check is unambiguous.
    ui_tx.send(UiAction::ShowHelp).expect("help");
    ui_tx.send(UiAction::GetState).expect("state");

    let mut help_seen = false;
    let mut state_seen_after_help = false;
    while !state_seen_after_help {
        let event = event_rx.recv().await.expect("event");
        if let AgentEvent::SystemMessage { text } = &event {
            if text.starts_with("Commands:") {
                assert!(!state_seen_after_help, "state must come after help");
                help_seen = true;
            } else if text.starts_with("Session:") && help_seen {
                state_seen_after_help = true;
            }
        }
    }

    ui_tx.send(UiAction::Quit).expect("quit");
    handle
        .await
        .expect("controller join")
        .expect("controller run");
}
