use std::{
    fs,
    path::Path,
    sync::{Mutex, MutexGuard},
};

use super::*;
use crate::bootstrap::build_tool_registry;
use crate::runtime_state::{RuntimeState, load_runtime_state_from};
use anie_protocol::{StopReason, ToolDef};
use anie_provider::{
    ApiKind, CostPerMillion, LlmContext, LlmMessage, ModelCompat, Provider, ProviderError,
    ProviderEvent, ProviderStream, StreamOptions,
    mock::{MockProvider, MockStreamScript},
};
use anie_session::SessionManager;
use futures::stream;
use tempfile::tempdir;

fn model(id: &str, provider: &str) -> Model {
    model_with_api(id, provider, ApiKind::OpenAICompletions)
}

fn model_with_api(id: &str, provider: &str, api: ApiKind) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        provider: provider.into(),
        api,
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

struct RecordingProvider {
    options: Arc<Mutex<Vec<StreamOptions>>>,
}

impl RecordingProvider {
    fn lock_options(&self) -> MutexGuard<'_, Vec<StreamOptions>> {
        self.options.lock().expect("recorded options")
    }
}

impl Provider for RecordingProvider {
    fn stream(
        &self,
        _model: &Model,
        _context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        self.lock_options().push(options);
        Ok(Box::pin(stream::iter(vec![Ok(ProviderEvent::Done(
            assistant_message("done"),
        ))])))
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
        messages
            .iter()
            .map(|message| LlmMessage {
                role: "user".into(),
                content: serde_json::to_value(message).expect("message json"),
            })
            .collect()
    }

    fn convert_tools(&self, _tools: &[ToolDef]) -> Vec<serde_json::Value> {
        Vec::new()
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
        compaction_stats: Arc::new(crate::compaction_stats::CompactionStatsAtomic::default()),
        harness_mode: crate::harness_mode::HarnessMode::default(),
        rlm_archived_messages: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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

fn controller_with_runtime_state_path(
    runtime_state_path: std::path::PathBuf,
) -> (InteractiveController, mpsc::Receiver<AgentEvent>) {
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
    let mut config_state = ConfigState::new(
        config.clone(),
        RuntimeState::default(),
        model("gpt-4o", "openai"),
        ThinkingLevel::Medium,
        None,
    );
    config_state.set_runtime_state_path_for_test(runtime_state_path);

    let state = ControllerState {
        config: config_state,
        session: SessionHandle::from_manager(session, sessions_dir, cwd),
        model_catalog: vec![model("gpt-4o", "openai"), model("gpt-4.1", "openai")],
        provider_registry: Arc::new(ProviderRegistry::new()),
        tool_registry,
        request_options_resolver: Arc::new(AuthResolver::new(None, config)),
        prompt_cache,
        retry_config: RetryConfig::default(),
        command_registry: crate::commands::CommandRegistry::with_builtins(),
        compaction_stats: Arc::new(crate::compaction_stats::CompactionStatsAtomic::default()),
        harness_mode: crate::harness_mode::HarnessMode::default(),
        rlm_archived_messages: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };

    let (_ui_action_tx, ui_action_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::channel(16);
    (
        InteractiveController::new(state, ui_action_rx, event_tx, false),
        event_rx,
    )
}

fn drain_system_messages(event_rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<String> {
    let mut messages = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        if let AgentEvent::SystemMessage { text } = event {
            messages.push(text);
        }
    }
    messages
}

#[test]
fn no_tools_flag_builds_empty_registry() {
    let registry = build_tool_registry(Path::new("."), true);
    assert!(registry.definitions().is_empty());
}

/// Plan `docs/rlm_2026-04-29/06_phased_implementation.md`
/// Phases A + C + F: the recurse tool, the virtualization
/// policy, and the background summarizer are all
/// installed only when `--harness-mode=rlm`. Other modes
/// get empty extras + no policy, so `build_agent` reuses
/// the bootstrap tool registry and the noop policy.
/// Tokio test because Phase F's `spawn_worker` requires a
/// runtime context.
#[tokio::test]
async fn build_rlm_extras_only_installs_recurse_in_rlm_mode() {
    use std::sync::atomic::AtomicU32;

    let (controller, _rx, _tx) = build_dispatch_controller(vec![model("gpt-4o", "openai")], 16);
    // Default mode is `current` — no recurse tool, no policy.
    let extras = build_rlm_extras(
        &controller.state,
        Arc::new(AtomicU32::new(8)),
        Vec::new(),
        None,
    );
    assert!(
        extras.tools.is_empty(),
        "current mode should not install rlm tools",
    );
    assert!(
        extras.policy.is_none(),
        "current mode should not install a policy",
    );

    // Flip to baseline — also no recurse, no policy.
    let mut controller = controller;
    controller.state.harness_mode = crate::harness_mode::HarnessMode::Baseline;
    let extras = build_rlm_extras(
        &controller.state,
        Arc::new(AtomicU32::new(8)),
        Vec::new(),
        None,
    );
    assert!(
        extras.tools.is_empty(),
        "baseline mode should not install rlm tools",
    );
    assert!(
        extras.policy.is_none(),
        "baseline mode should not install a policy",
    );

    // Flip to rlm — exactly one tool (recurse) and a policy.
    controller.state.harness_mode = crate::harness_mode::HarnessMode::Rlm;
    let extras = build_rlm_extras(
        &controller.state,
        Arc::new(AtomicU32::new(8)),
        Vec::new(),
        None,
    );
    assert_eq!(
        extras.tools.len(),
        1,
        "rlm mode should install exactly one extra tool"
    );
    assert_eq!(extras.tools[0].definition().name, "recurse");
    assert!(
        extras.policy.is_some(),
        "rlm mode should install the virtualization policy"
    );
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
            .filter(|event| matches!(event, AgentEvent::CompactionStart { .. }))
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

    // Plan 06 PR A regression guard: the reactive overflow
    // path must tag its `CompactionStart` / `CompactionEnd`
    // pair with `CompactionPhase::ReactiveOverflow`, never
    // `PrePrompt`.
    let reactive_starts = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AgentEvent::CompactionStart {
                    phase: anie_protocol::CompactionPhase::ReactiveOverflow,
                },
            )
        })
        .count();
    assert_eq!(
        reactive_starts, 1,
        "overflow path must emit exactly one ReactiveOverflow CompactionStart",
    );
    let reactive_ends = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AgentEvent::CompactionEnd {
                    phase: anie_protocol::CompactionPhase::ReactiveOverflow,
                    ..
                },
            )
        })
        .count();
    assert_eq!(
        reactive_ends, 1,
        "overflow path must emit exactly one ReactiveOverflow CompactionEnd",
    );
}

/// Plan 06 PR B: `/state` should render a "Compactions this
/// session" block showing the per-phase counts.
#[test]
fn state_summary_includes_compaction_counts_block() {
    let stats = crate::compaction_stats::CompactionStats {
        pre_prompt: 2,
        mid_turn: 1,
        reactive_overflow: 0,
    };
    let summary = format_state_summary(
        &ollama_model(),
        ThinkingLevel::Medium,
        None,
        None,
        32_768,
        "session-stats",
        None,
        None,
        stats,
    );

    assert!(
        summary.contains("Compactions this session"),
        "missing compactions block: {summary}",
    );
    assert!(
        summary.contains("Total: 3"),
        "expected 'Total: 3': {summary}",
    );
    assert!(
        summary.contains("pre-prompt: 2"),
        "expected pre-prompt count: {summary}",
    );
    assert!(
        summary.contains("mid-turn: 1"),
        "expected mid-turn count: {summary}",
    );
    assert!(
        summary.contains("overflow: 0"),
        "expected overflow count: {summary}",
    );
    // Plan 06 PR B: counters are documented as
    // this-process-lifetime to head off questions about why
    // they zero on `--continue`.
    assert!(
        summary.contains("this process only"),
        "expected process-lifetime note: {summary}",
    );
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
            .filter(|event| matches!(event, AgentEvent::CompactionStart { .. }))
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
    build_dispatch_controller_with_runtime_state(catalog, event_capacity, RuntimeState::default())
}

fn build_dispatch_controller_with_runtime_state(
    catalog: Vec<Model>,
    event_capacity: usize,
    runtime_state: RuntimeState,
) -> (
    InteractiveController,
    mpsc::Receiver<AgentEvent>,
    mpsc::UnboundedSender<UiAction>,
) {
    build_dispatch_controller_with_runtime_state_path(catalog, event_capacity, runtime_state, None)
}

fn build_dispatch_controller_with_runtime_state_path(
    catalog: Vec<Model>,
    event_capacity: usize,
    runtime_state: RuntimeState,
    runtime_state_path: Option<std::path::PathBuf>,
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

    let mut config_state = ConfigState::new(
        config.clone(),
        runtime_state,
        default_model.clone(),
        ThinkingLevel::Medium,
        None,
    );
    if let Some(path) = runtime_state_path {
        config_state.set_runtime_state_path_for_test(path);
    }

    let state = ControllerState {
        config: config_state,
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
        compaction_stats: Arc::new(crate::compaction_stats::CompactionStatsAtomic::default()),
        harness_mode: crate::harness_mode::HarnessMode::default(),
        rlm_archived_messages: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };

    let (ui_action_tx, ui_action_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::channel(event_capacity);
    let controller = InteractiveController::new(state, ui_action_rx, event_tx, false);

    (controller, event_rx, ui_action_tx)
}

fn build_state_with_registry(
    model: Model,
    runtime_state: RuntimeState,
    provider_registry: ProviderRegistry,
) -> ControllerState {
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

    ControllerState {
        config: ConfigState::new(
            config.clone(),
            runtime_state,
            model.clone(),
            ThinkingLevel::Medium,
            None,
        ),
        session: SessionHandle::from_manager(session, sessions_dir, cwd),
        model_catalog: vec![model],
        provider_registry: Arc::new(provider_registry),
        tool_registry,
        request_options_resolver: Arc::new(AuthResolver::new(None, config)),
        prompt_cache,
        retry_config: RetryConfig::default(),
        command_registry: crate::commands::CommandRegistry::with_builtins(),
        compaction_stats: Arc::new(crate::compaction_stats::CompactionStatsAtomic::default()),
        harness_mode: crate::harness_mode::HarnessMode::default(),
        rlm_archived_messages: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    }
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

#[test]
fn compaction_strategy_uses_effective_ollama_context_window() {
    let mut runtime_state = RuntimeState::default();
    runtime_state
        .ollama_num_ctx_overrides
        .insert("ollama:qwen3:32b".into(), 16_384);
    let ollama_model = model_with_api("qwen3:32b", "ollama", ApiKind::OllamaChatApi);
    let (controller, _event_rx, _tx) =
        build_dispatch_controller_with_runtime_state(vec![ollama_model], 16, runtime_state);

    let (config, _strategy) = controller.state.compaction_strategy(2_000);

    assert_eq!(config.context_window, 16_384);
    assert_eq!(config.keep_recent_tokens, 2_000);
}

/// PR 2.2 of `docs/active_input_2026-04-27/`. While a run is
/// active, `UiAction::QueuePrompt(text)` pushes onto the
/// controller's FIFO queue and emits a "Queued follow-up #N"
/// system message. The drain at the run-completion boundary
/// (covered by `queued_prompt_runs_after_current_run_finishes`)
/// is a separate concern; this test pins the storage path.
#[tokio::test]
async fn queue_prompt_appends_to_fifo_queue_while_active() {
    use anie_agent::AgentRunResult;
    use tokio_util::sync::CancellationToken;

    let (mut controller, mut event_rx, _tx) =
        build_dispatch_controller(vec![model("gpt-4o", "openai")], 16);

    // Fake an in-flight run by constructing a `CurrentRun` whose
    // handle resolves immediately to a default result. The
    // QueuePrompt handler only checks `current_run.is_some()`,
    // not whether the underlying task is still pending, so this
    // is sufficient for testing the storage path.
    let handle = tokio::spawn(async {
        AgentRunResult {
            generated_messages: Vec::new(),
            final_context: Vec::new(),
            terminal_error: None,
        }
    });
    controller.current_run = Some(CurrentRun {
        handle,
        cancel: CancellationToken::new(),
        already_compacted: false,
        retry_attempt: 0,
    });

    assert!(
        controller
            .try_handle_action(UiAction::QueuePrompt("first".into()))
            .await
            .is_ok()
    );
    assert!(
        controller
            .try_handle_action(UiAction::QueuePrompt("second".into()))
            .await
            .is_ok()
    );

    let queued: Vec<_> = controller.queued_prompts.iter().cloned().collect();
    assert_eq!(queued, vec!["first".to_string(), "second".to_string()]);

    // Both prompts should have produced a system message
    // acknowledging the queue position.
    let mut acks = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        if let AgentEvent::SystemMessage { text } = event
            && text.starts_with("Queued follow-up")
        {
            acks.push(text);
        }
    }
    assert_eq!(acks.len(), 2, "expected two queue-ack messages: {acks:?}");
    assert!(acks[0].contains("#1"), "first ack: {}", acks[0]);
    assert!(acks[1].contains("#2"), "second ack: {}", acks[1]);
}

/// PR 2.4 of `docs/active_input_2026-04-27/`. The
/// run-completion drain emits "Starting queued follow-up: …"
/// before invoking `start_prompt_run`, so users can tell when
/// the next queued prompt is actually beginning rather than
/// guessing from a fresh prompt prefix in the transcript.
#[tokio::test]
async fn drain_queued_prompt_emits_starting_system_message() {
    let (mut controller, mut event_rx, _tx) =
        build_dispatch_controller(vec![model("gpt-4o", "openai")], 16);

    // Pre-load the queue. `try_drain_queued_prompt` will pop
    // the front entry and call `start_prompt_run`, which fails
    // in this minimal harness (no provider registered) — but
    // the system message must have already been emitted before
    // the failed start, so the assertion still holds.
    controller
        .queued_prompts
        .push_back("explain the architecture".into());

    let _ = controller.try_drain_queued_prompt().await;

    let mut saw_starting_message = false;
    while let Ok(event) = event_rx.try_recv() {
        if let AgentEvent::SystemMessage { text } = event
            && text.starts_with("Starting queued follow-up:")
        {
            assert!(text.contains("explain the architecture"));
            saw_starting_message = true;
        }
    }
    assert!(
        saw_starting_message,
        "drain must surface a 'Starting queued follow-up' system message",
    );
}

/// PR 2.3 of `docs/active_input_2026-04-27/`. When a
/// `UiAction::QueuePrompt` arrives while a transient retry is
/// armed, the controller cancels the retry and routes the
/// prompt through `start_prompt_run`. Stale automatic retries
/// must not hide fresh user input.
#[tokio::test]
async fn queue_prompt_cancels_pending_retry() {
    use tokio::time::Instant as TokioInstant;

    let (mut controller, mut event_rx, _tx) =
        build_dispatch_controller(vec![model("gpt-4o", "openai")], 16);

    // Arm a retry. Deadline far enough out that
    // `start_prompt_run`'s own logic doesn't race the timer.
    controller.pending_retry = PendingRetry::Armed {
        deadline: TokioInstant::now() + Duration::from_secs(60),
        attempt: 1,
        already_compacted: false,
        error: ProviderError::Transport("test transient".into()),
        provider: "test-provider".into(),
        model: "test-model".into(),
    };

    // No mock provider is registered for the model's API in
    // this minimal harness, so `start_prompt_run` errors —
    // but the retry must still have been cleared.
    let _ = controller
        .try_handle_action(UiAction::QueuePrompt("urgent".into()))
        .await;

    assert!(
        matches!(controller.pending_retry, PendingRetry::Idle),
        "pending retry must be cleared when QueuePrompt arrives during armed backoff",
    );
    let mut saw_cancel_message = false;
    while let Ok(event) = event_rx.try_recv() {
        if let AgentEvent::SystemMessage { text } = event
            && text.contains("Cancelling pending retry")
        {
            saw_cancel_message = true;
        }
    }
    assert!(
        saw_cancel_message,
        "user must see why the retry was dropped",
    );
}

/// Plan `docs/run_abort_breadcrumb_2026-04-28/`: count
/// assistant messages persisted to the session log whose body
/// matches the canceled retry's error string. Used by every
/// breadcrumb test.
fn count_error_assistant_breadcrumbs(controller: &InteractiveController, needle: &str) -> usize {
    controller
        .state
        .session
        .inner()
        .entries()
        .iter()
        .filter(|entry| match entry {
            anie_session::SessionEntry::Message {
                message: anie_protocol::Message::Assistant(assistant),
                ..
            } => {
                assistant
                    .error_message
                    .as_deref()
                    .is_some_and(|m| m.contains(needle))
                    && matches!(assistant.stop_reason, anie_protocol::StopReason::Error)
            }
            _ => false,
        })
        .count()
}

/// Plan `docs/run_abort_breadcrumb_2026-04-28/`: a fresh prompt
/// arriving while a retry is armed must finalize the failed
/// turn with an error-assistant breadcrumb in the session log
/// — preventing back-to-back user messages.
#[tokio::test]
async fn pending_retry_canceled_by_new_prompt_writes_error_assistant_breadcrumb() {
    use tokio::time::Instant as TokioInstant;

    let (mut controller, _event_rx, _tx) =
        build_dispatch_controller(vec![model("gpt-4o", "openai")], 16);

    controller.pending_retry = PendingRetry::Armed {
        deadline: TokioInstant::now() + Duration::from_secs(60),
        attempt: 1,
        already_compacted: false,
        error: ProviderError::RateLimited {
            retry_after_ms: None,
        },
        provider: "openrouter".into(),
        model: "minimax/minimax-m2.5:free".into(),
    };

    let _ = controller
        .try_handle_action(UiAction::QueuePrompt("hello".into()))
        .await;

    assert!(
        matches!(controller.pending_retry, PendingRetry::Idle),
        "retry must be cleared",
    );
    assert_eq!(
        count_error_assistant_breadcrumbs(&controller, "Rate limited"),
        1,
        "breadcrumb must be persisted to the session for the canceled retry",
    );
}

/// Plan `docs/run_abort_breadcrumb_2026-04-28/`: explicit user
/// abort writes the breadcrumb just like a new prompt does.
#[tokio::test]
async fn pending_retry_canceled_by_abort_writes_error_assistant_breadcrumb() {
    use tokio::time::Instant as TokioInstant;

    let (mut controller, _event_rx, _tx) =
        build_dispatch_controller(vec![model("gpt-4o", "openai")], 16);

    controller.pending_retry = PendingRetry::Armed {
        deadline: TokioInstant::now() + Duration::from_secs(60),
        attempt: 1,
        already_compacted: false,
        error: ProviderError::Transport("dns".into()),
        provider: "openai".into(),
        model: "gpt-4o".into(),
    };

    controller.try_handle_action(UiAction::Abort).await.ok();

    assert!(matches!(controller.pending_retry, PendingRetry::Idle));
    assert_eq!(
        count_error_assistant_breadcrumbs(&controller, "Transport error"),
        1,
        "abort must persist a breadcrumb for the canceled retry",
    );
}

/// Plan `docs/run_abort_breadcrumb_2026-04-28/`: quit during a
/// pending retry still writes the breadcrumb so the partially-
/// finished session is well-formed on next resume.
#[tokio::test]
async fn pending_retry_canceled_by_quit_writes_error_assistant_breadcrumb() {
    use tokio::time::Instant as TokioInstant;

    let (mut controller, _event_rx, _tx) =
        build_dispatch_controller(vec![model("gpt-4o", "openai")], 16);

    controller.pending_retry = PendingRetry::Armed {
        deadline: TokioInstant::now() + Duration::from_secs(60),
        attempt: 1,
        already_compacted: false,
        error: ProviderError::Transport("hangup".into()),
        provider: "openai".into(),
        model: "gpt-4o".into(),
    };

    controller.try_handle_action(UiAction::Quit).await.ok();

    assert!(matches!(controller.pending_retry, PendingRetry::Idle));
    assert_eq!(
        count_error_assistant_breadcrumbs(&controller, "Transport error"),
        1,
        "quit must persist a breadcrumb for the canceled retry",
    );
}

/// Plan `docs/run_abort_breadcrumb_2026-04-28/`: a model switch
/// (or any run-affecting config change) cancels the pending
/// retry. The breadcrumb must be attributed to the *original*
/// failed run's provider/model, not the freshly-selected one.
#[tokio::test]
async fn pending_retry_canceled_by_model_switch_writes_breadcrumb_with_original_model() {
    use tokio::time::Instant as TokioInstant;

    let (mut controller, _event_rx, _tx) = build_dispatch_controller(
        vec![
            model("gpt-4o", "openai"),
            model("claude-sonnet-4.6", "anthropic"),
        ],
        16,
    );

    controller.pending_retry = PendingRetry::Armed {
        deadline: TokioInstant::now() + Duration::from_secs(60),
        attempt: 1,
        already_compacted: false,
        error: ProviderError::Transport("timeout".into()),
        provider: "openai".into(),
        model: "gpt-4o".into(),
    };

    controller
        .cancel_pending_retry_for_run_affecting_change()
        .await
        .expect("cancel ok");

    assert!(matches!(controller.pending_retry, PendingRetry::Idle));
    let entries = controller.state.session.inner().entries();
    let breadcrumb = entries.iter().find_map(|entry| match entry {
        anie_session::SessionEntry::Message { message, .. } => match message {
            anie_protocol::Message::Assistant(assistant)
                if matches!(assistant.stop_reason, anie_protocol::StopReason::Error) =>
            {
                Some(assistant)
            }
            _ => None,
        },
        _ => None,
    });
    let breadcrumb = breadcrumb.expect("breadcrumb persisted");
    assert_eq!(
        breadcrumb.provider, "openai",
        "breadcrumb provider must match the failed run, not the freshly-selected one",
    );
    assert_eq!(breadcrumb.model, "gpt-4o");
}

/// Plan `docs/run_abort_breadcrumb_2026-04-28/`: when the
/// retry state is already `Idle`, calling the helper must be
/// a no-op and not write a spurious assistant message.
#[tokio::test]
async fn abort_pending_retry_is_noop_when_idle() {
    let (mut controller, _event_rx, _tx) =
        build_dispatch_controller(vec![model("gpt-4o", "openai")], 16);

    assert!(matches!(controller.pending_retry, PendingRetry::Idle));

    controller
        .abort_pending_retry()
        .await
        .expect("idle helper succeeds");

    let assistant_count = controller
        .state
        .session
        .inner()
        .entries()
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                anie_session::SessionEntry::Message {
                    message: anie_protocol::Message::Assistant(_),
                    ..
                },
            )
        })
        .count();
    assert_eq!(
        assistant_count, 0,
        "no-op helper must not synthesize a breadcrumb",
    );
}

/// PR 8.2 of `docs/midturn_compaction_2026-04-27/`. Every
/// `start_prompt_run` resets the per-turn compaction budget
/// to the configured `max_per_turn`. Without this, a previous
/// turn that consumed budget would silently constrain the
/// next turn — undermining the semantic that each user turn
/// gets its own allowance.
#[tokio::test]
async fn controller_compaction_budget_resets_to_max_per_turn_on_run_prompt() {
    use std::sync::atomic::Ordering;

    let (mut controller, _event_rx, _tx) =
        build_dispatch_controller(vec![model("gpt-4o", "openai")], 16);

    // Drain the budget to zero, simulating a prior turn that
    // exhausted its compactions.
    controller
        .compactions_remaining_this_turn
        .store(0, Ordering::Release);

    // No mock provider for this model's API in the harness, so
    // `start_prompt_run` errors after the reset path runs —
    // but the reset happens before that error, so the value
    // we assert below is the post-reset state regardless.
    let _ = controller.start_prompt_run("hello".into()).await;

    // Default `max_per_turn` is 8 (raised 2026-04-29; see
    // `anie_config::CompactionConfig`). The exact number isn't
    // the contract under test — what matters is the reset, so
    // the assertion reads the live default rather than hard-
    // coding it again.
    let expected = anie_config::CompactionConfig::default().max_per_turn;
    assert_eq!(
        controller
            .compactions_remaining_this_turn
            .load(Ordering::Acquire),
        expected,
        "fresh user turn must restore the configured per-turn allowance",
    );
}

/// PR 7.1 of `docs/active_input_2026-04-27/`. While a run is
/// active, `AbortAndQueuePrompt(text)` must:
///  - push `text` to the **front** of `queued_prompts` so it
///    runs ahead of any FIFO follow-ups already queued;
///  - cancel the in-flight run via `CurrentRun::cancel`;
///  - emit a system message announcing the abort + queued
///    draft so the user has a clear log of what happened.
#[tokio::test]
async fn abort_and_queue_during_active_run_front_queues_and_cancels() {
    use anie_agent::AgentRunResult;
    use tokio_util::sync::CancellationToken;

    let (mut controller, mut event_rx, _tx) =
        build_dispatch_controller(vec![model("gpt-4o", "openai")], 16);

    // Pre-seed a stale FIFO queue entry so we can prove the new
    // interrupt arrives at the front, not the back.
    controller
        .queued_prompts
        .push_back("stale follow-up".into());

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async {
        AgentRunResult {
            generated_messages: Vec::new(),
            final_context: Vec::new(),
            terminal_error: None,
        }
    });
    controller.current_run = Some(CurrentRun {
        handle,
        cancel,
        already_compacted: false,
        retry_attempt: 0,
    });

    assert!(
        controller
            .try_handle_action(UiAction::AbortAndQueuePrompt("interrupt me".into()))
            .await
            .is_ok()
    );

    let queued: Vec<_> = controller.queued_prompts.iter().cloned().collect();
    assert_eq!(
        queued,
        vec!["interrupt me".to_string(), "stale follow-up".to_string()],
        "interrupt must front-queue ahead of any existing follow-ups",
    );
    assert!(
        cancel_clone.is_cancelled(),
        "abort-and-queue must cancel the current run",
    );

    let mut saw_message = false;
    while let Ok(event) = event_rx.try_recv() {
        if let AgentEvent::SystemMessage { text } = event
            && text.starts_with("Aborting current run")
        {
            assert!(text.contains("interrupt me"), "got: {text}");
            saw_message = true;
        }
    }
    assert!(saw_message, "user must see why the run is aborting");
}

/// PR 7.1 of `docs/active_input_2026-04-27/`. When the
/// transient-error retry timer is armed, an interrupt-and-send
/// must clear the retry and start the new prompt immediately —
/// same precedence rule the FIFO `QueuePrompt` already
/// enforces (a fresh user signal beats a stale automatic
/// retry).
#[tokio::test]
async fn abort_and_queue_during_pending_retry_clears_retry() {
    use tokio::time::Instant as TokioInstant;

    let (mut controller, mut event_rx, _tx) =
        build_dispatch_controller(vec![model("gpt-4o", "openai")], 16);

    controller.pending_retry = PendingRetry::Armed {
        deadline: TokioInstant::now() + Duration::from_secs(60),
        attempt: 1,
        already_compacted: false,
        error: ProviderError::Transport("test transient".into()),
        provider: "test-provider".into(),
        model: "test-model".into(),
    };

    // No mock provider is registered for the model's API in
    // this minimal harness, so `start_prompt_run` errors out —
    // but the retry must still have been cleared.
    let _ = controller
        .try_handle_action(UiAction::AbortAndQueuePrompt("interrupt".into()))
        .await;

    assert!(
        matches!(controller.pending_retry, PendingRetry::Idle),
        "pending retry must be cleared when AbortAndQueuePrompt arrives during armed backoff",
    );
    let mut saw_cancel_message = false;
    while let Ok(event) = event_rx.try_recv() {
        if let AgentEvent::SystemMessage { text } = event
            && text.contains("Cancelling pending retry")
        {
            saw_cancel_message = true;
        }
    }
    assert!(
        saw_cancel_message,
        "user must see why the retry was dropped",
    );
}

/// PR 7.1 of `docs/active_input_2026-04-27/`. With nothing
/// active, `AbortAndQueuePrompt` is equivalent to a direct
/// submit — there's no run to abort, so we just start the
/// prompt. Pinning this avoids a regression where the action
/// silently no-ops on an empty controller (the user's draft
/// would vanish).
#[tokio::test]
async fn abort_and_queue_when_idle_starts_immediately() {
    let (mut controller, _event_rx, _tx) =
        build_dispatch_controller(vec![model("gpt-4o", "openai")], 16);

    assert!(controller.current_run.is_none());

    // No mock provider is registered, so `start_prompt_run`
    // returns an error — but it WILL be invoked, which is
    // what we're testing. The queue must remain empty either
    // way; we don't queue against ourselves.
    let _ = controller
        .try_handle_action(UiAction::AbortAndQueuePrompt("interrupt".into()))
        .await;

    assert!(
        controller.queued_prompts.is_empty(),
        "idle AbortAndQueuePrompt must not enqueue; it should start the run directly",
    );
}

/// PR 2.2 of `docs/active_input_2026-04-27/`. When a queued
/// prompt arrives while no run is active, it starts the prompt
/// directly (matches `SubmitPrompt` shape — same end result for
/// the user). The queue stays empty because there's nothing to
/// queue against.
#[tokio::test]
async fn queue_prompt_starts_immediately_when_idle() {
    let (mut controller, _event_rx, _tx) =
        build_dispatch_controller(vec![model("gpt-4o", "openai")], 16);

    assert!(controller.current_run.is_none());

    // No mock provider is registered for the model's API in
    // this minimal harness, so `start_prompt_run` will error
    // out — but it WILL be invoked, which is what we're
    // testing. The queue must remain empty either way.
    let _ = controller
        .try_handle_action(UiAction::QueuePrompt("hi".into()))
        .await;

    assert!(
        controller.queued_prompts.is_empty(),
        "idle QueuePrompt must not enqueue; it should start the run directly",
    );
}

/// Regression for PR 1.3 of `docs/code_review_2026-04-27/`. The
/// give-up handler in `run_prompt` must pass
/// `effective_ollama_context_window()` (the value actually sent on
/// the wire) into `render_user_facing_provider_error`, not the
/// raw `model.context_window`. Otherwise a user with an active
/// `/context-length` override sees the wrong values in the failure
/// message.
///
/// Pinned at the helper boundary rather than driving a full run:
/// the bug's blast radius is exactly the value passed at the call
/// site, and the renderer's formula is already covered by
/// `user_error::tests::*`. If a future change reverts the call
/// site to `model.context_window`, this test catches it on the
/// next compile because the assertion would fail (16384 vs 262144).
#[test]
fn give_up_handler_renders_with_effective_ollama_context_window() {
    let mut runtime_state = RuntimeState::default();
    runtime_state
        .ollama_num_ctx_overrides
        .insert("ollama:qwen3:32b".into(), 16_384);
    let ollama_model = model_with_api("qwen3:32b", "ollama", ApiKind::OllamaChatApi);
    let (controller, _event_rx, _tx) =
        build_dispatch_controller_with_runtime_state(vec![ollama_model], 16, runtime_state);

    let error = ProviderError::ModelLoadResources {
        body: "model requires more system memory".into(),
        suggested_num_ctx: 8_192,
    };
    let model = controller.state.config.current_model();
    let requested_num_ctx = controller.state.config.effective_ollama_context_window();
    let message = crate::user_error::render_user_facing_provider_error(
        &error,
        requested_num_ctx,
        &model.provider,
        &model.id,
    )
    .expect("ModelLoadResources should render a user-facing message");

    // The override value (16384) must appear; the model's raw
    // context_window (262144) must not.
    assert!(
        message.contains("num_ctx=16384"),
        "message should report the override: {message}"
    );
    assert!(
        !message.contains("num_ctx=262144"),
        "message should NOT report the raw model.context_window: {message}"
    );
    // Halved attempt is 16384/2 = 8192 (the override's halve), not
    // 262144/2 = 131072 (which would be wrong).
    assert!(
        message.contains("num_ctx=8192"),
        "message should report the halved override: {message}"
    );
}

#[test]
fn status_event_uses_effective_ollama_context_window() {
    let mut runtime_state = RuntimeState::default();
    runtime_state
        .ollama_num_ctx_overrides
        .insert("ollama:qwen3:32b".into(), 16_384);
    let ollama_model = model_with_api("qwen3:32b", "ollama", ApiKind::OllamaChatApi);
    let (controller, _event_rx, _tx) =
        build_dispatch_controller_with_runtime_state(vec![ollama_model], 16, runtime_state);

    let event = controller.state.status_event();

    assert!(matches!(
        event,
        AgentEvent::StatusUpdate {
            context_window: 16_384,
            ..
        }
    ));
}

#[tokio::test]
async fn build_agent_snapshots_num_ctx_override_into_agent_loop_config() {
    let recorded_options = Arc::new(Mutex::new(Vec::new()));
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(
        ApiKind::OllamaChatApi,
        Box::new(RecordingProvider {
            options: Arc::clone(&recorded_options),
        }),
    );
    let mut runtime_state = RuntimeState::default();
    runtime_state
        .ollama_num_ctx_overrides
        .insert("ollama:qwen3:32b".into(), 16_384);
    let state = build_state_with_registry(
        model_with_api("qwen3:32b", "ollama", ApiKind::OllamaChatApi),
        runtime_state,
        provider_registry,
    );
    let agent = build_agent(&state, None, RlmExtras::empty());
    let (event_tx, _event_rx) = mpsc::channel(16);

    let result = agent
        .run(
            vec![user_message("hello")],
            Vec::new(),
            event_tx,
            CancellationToken::new(),
        )
        .await;

    assert!(result.terminal_error.is_none());
    let options = recorded_options.lock().expect("recorded options");
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].num_ctx_override, Some(16_384));
}

fn ollama_model() -> Model {
    model_with_api("qwen3:32b", "ollama", ApiKind::OllamaChatApi)
}

fn controller_for_context_length_test(
    runtime_state: RuntimeState,
) -> (
    tempfile::TempDir,
    InteractiveController,
    mpsc::Receiver<AgentEvent>,
    mpsc::UnboundedSender<UiAction>,
) {
    let tempdir = tempdir().expect("tempdir");
    let runtime_state_path = tempdir.path().join("state.json");
    let (controller, event_rx, ui_tx) = build_dispatch_controller_with_runtime_state_path(
        vec![ollama_model()],
        32,
        runtime_state,
        Some(runtime_state_path),
    );
    (tempdir, controller, event_rx, ui_tx)
}

/// Variant of `controller_for_context_length_test` that sets a
/// workspace `[ollama] default_max_num_ctx` cap on the
/// underlying `AnieConfig`. Used by Cap PR 3 messaging tests.
fn controller_for_context_length_test_with_cap(
    runtime_state: RuntimeState,
    cap: u64,
) -> (
    tempfile::TempDir,
    InteractiveController,
    mpsc::Receiver<AgentEvent>,
    mpsc::UnboundedSender<UiAction>,
) {
    let tempdir = tempdir().expect("tempdir");
    let cwd = tempdir.path().join("cwd");
    let sessions_dir = tempdir.path().join("sessions");
    let runtime_state_path = tempdir.path().join("state.json");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(&sessions_dir).expect("create sessions dir");

    let mut config = AnieConfig::default();
    config.ollama.default_max_num_ctx = Some(cap);

    let tool_registry = build_tool_registry(&cwd, true);
    let prompt_cache =
        SystemPromptCache::build(&cwd, &tool_registry, &config).expect("build prompt cache");
    let session = SessionManager::new_session(&sessions_dir, &cwd).expect("new session");
    let default_model = ollama_model();

    let mut config_state = ConfigState::new(
        config.clone(),
        runtime_state,
        default_model.clone(),
        ThinkingLevel::Medium,
        None,
    );
    config_state.set_runtime_state_path_for_test(runtime_state_path);

    let state = ControllerState {
        config: config_state,
        session: SessionHandle::from_manager(session, sessions_dir, cwd),
        model_catalog: vec![default_model],
        provider_registry: Arc::new(ProviderRegistry::new()),
        tool_registry,
        request_options_resolver: Arc::new(AuthResolver::new(None, config)),
        prompt_cache,
        retry_config: RetryConfig::default(),
        command_registry: crate::commands::CommandRegistry::with_builtins(),
        compaction_stats: Arc::new(crate::compaction_stats::CompactionStatsAtomic::default()),
        harness_mode: crate::harness_mode::HarnessMode::default(),
        rlm_archived_messages: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };

    let (ui_action_tx, ui_action_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::channel(32);
    let controller = InteractiveController::new(state, ui_action_rx, event_tx, false);
    (tempdir, controller, event_rx, ui_action_tx)
}

#[tokio::test]
async fn context_length_sets_override_for_current_ollama_model() {
    let (_tempdir, mut controller, mut event_rx, _tx) =
        controller_for_context_length_test(RuntimeState::default());

    controller
        .handle_action(UiAction::ContextLength(Some("16384".into())))
        .await
        .expect("set context length");

    assert_eq!(
        controller.state.config.active_ollama_num_ctx_override(),
        Some(16_384)
    );
    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(msg.contains("Context window set to 16 384"), "{msg}");
}

#[tokio::test]
async fn context_length_reset_clears_override() {
    let mut runtime_state = RuntimeState::default();
    runtime_state
        .ollama_num_ctx_overrides
        .insert("ollama:qwen3:32b".into(), 16_384);
    let (_tempdir, mut controller, mut event_rx, _tx) =
        controller_for_context_length_test(runtime_state);

    controller
        .handle_action(UiAction::ContextLength(Some("reset".into())))
        .await
        .expect("reset context length");

    assert_eq!(
        controller.state.config.active_ollama_num_ctx_override(),
        None
    );
    assert_eq!(
        controller.state.config.effective_ollama_context_window(),
        32_768
    );
    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(msg.contains("Context window reset to 32 768"), "{msg}");
}

#[tokio::test]
async fn context_length_on_non_ollama_model_emits_friendly_error() {
    let (mut controller, mut event_rx, _tx) = build_dispatch_controller(Vec::new(), 16);

    controller
        .handle_action(UiAction::ContextLength(Some("16384".into())))
        .await
        .expect("reject non-Ollama model");

    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(
        msg.contains("only applies to Ollama native /api/chat models")
            && msg.contains("openai:gpt-4o"),
        "{msg}"
    );
}

#[tokio::test]
async fn context_length_rejects_out_of_range_value() {
    let (_tempdir, mut controller, mut event_rx, _tx) =
        controller_for_context_length_test(RuntimeState::default());

    controller
        .handle_action(UiAction::ContextLength(Some("1024".into())))
        .await
        .expect("reject out of range");

    assert_eq!(
        controller.state.config.active_ollama_num_ctx_override(),
        None
    );
    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(
        msg.contains("Invalid context length 1024") && msg.contains("2048"),
        "{msg}"
    );
}

#[tokio::test]
async fn context_length_rejects_unparseable_argument() {
    let (_tempdir, mut controller, mut event_rx, _tx) =
        controller_for_context_length_test(RuntimeState::default());

    controller
        .handle_action(UiAction::ContextLength(Some("wide".into())))
        .await
        .expect("reject unparseable");

    assert_eq!(
        controller.state.config.active_ollama_num_ctx_override(),
        None
    );
    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(
        msg.contains("Invalid context length 'wide'") && msg.contains("reset"),
        "{msg}"
    );
}

#[tokio::test]
async fn context_length_set_rejected_while_run_active() {
    let (_tempdir, mut controller, mut event_rx, _tx) =
        controller_for_context_length_test(RuntimeState::default());
    controller.current_run = Some(CurrentRun {
        handle: tokio::spawn(async { anie_agent::AgentRunResult::default() }),
        cancel: CancellationToken::new(),
        already_compacted: false,
        retry_attempt: 0,
    });

    controller
        .handle_action(UiAction::ContextLength(Some("16384".into())))
        .await
        .expect("reject while active");

    assert_eq!(
        controller.state.config.active_ollama_num_ctx_override(),
        None
    );
    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(msg.contains("run is active"), "{msg}");
}

#[tokio::test]
async fn context_length_set_rejected_while_retry_pending() {
    let (_tempdir, mut controller, mut event_rx, _tx) =
        controller_for_context_length_test(RuntimeState::default());
    controller.pending_retry = PendingRetry::Armed {
        deadline: Instant::now() + Duration::from_secs(60),
        attempt: 1,
        already_compacted: false,
        error: ProviderError::Transport("test transient".into()),
        provider: "test-provider".into(),
        model: "test-model".into(),
    };

    controller
        .handle_action(UiAction::ContextLength(Some("16384".into())))
        .await
        .expect("reject while retry pending");

    assert_eq!(
        controller.state.config.active_ollama_num_ctx_override(),
        None
    );
    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(msg.contains("retry is pending"), "{msg}");
}

#[tokio::test]
async fn context_length_no_args_reports_current_effective_value_and_source() {
    let mut runtime_state = RuntimeState::default();
    runtime_state
        .ollama_num_ctx_overrides
        .insert("ollama:qwen3:32b".into(), 16_384);
    let (_tempdir, mut controller, mut event_rx, _tx) =
        controller_for_context_length_test(runtime_state);

    controller
        .handle_action(UiAction::ContextLength(None))
        .await
        .expect("query context length");

    let msg = drain_next_system_message(&mut event_rx).await;
    assert_eq!(
        msg,
        "Current context window: 16 384 (runtime override; baseline 32 768)"
    );
}

#[tokio::test]
async fn context_length_no_args_message_includes_cap_when_capped() {
    // Cap PR 3: when [ollama] default_max_num_ctx is set and
    // no runtime override is active, the no-args /context-length
    // message must disclose the cap so the user understands why
    // the value isn't the model's architectural max.
    let (_tempdir, mut controller, mut event_rx, _tx) =
        controller_for_context_length_test_with_cap(RuntimeState::default(), 32_768);

    controller
        .handle_action(UiAction::ContextLength(None))
        .await
        .expect("query context length");

    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(
        msg.contains("workspace cap"),
        "message must disclose the cap; got:\n{msg}"
    );
    assert!(
        msg.contains("[ollama] default_max_num_ctx"),
        "message must name the config field so users know how to change it; got:\n{msg}"
    );
}

#[tokio::test]
async fn context_length_no_args_message_omits_cap_when_no_cap_set() {
    // Boundary: when no cap is configured, the message stays
    // simple — don't add cap-related noise to the default
    // experience.
    let (_tempdir, mut controller, mut event_rx, _tx) =
        controller_for_context_length_test(RuntimeState::default());

    controller
        .handle_action(UiAction::ContextLength(None))
        .await
        .expect("query context length");

    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(
        !msg.contains("workspace cap"),
        "no cap → no cap-related text; got:\n{msg}"
    );
    assert!(
        !msg.contains("default_max_num_ctx"),
        "no cap → no cap-field-name; got:\n{msg}"
    );
}

#[tokio::test]
async fn context_length_set_above_cap_emits_warning_but_applies_override() {
    // Cap PR 3: a runtime override that exceeds the workspace
    // cap still applies (user intent wins) but produces a
    // warning so the conflict is visible. Otherwise users
    // wouldn't know why their override might still hit a load
    // failure on the wire.
    let (_tempdir, mut controller, mut event_rx, _tx) =
        controller_for_context_length_test_with_cap(RuntimeState::default(), 32_768);

    controller
        .handle_action(UiAction::ContextLength(Some("65536".into())))
        .await
        .expect("set context length");

    let success_msg = drain_next_system_message(&mut event_rx).await;
    assert!(
        success_msg.contains("65 536"),
        "first message confirms the override applied at the requested value; got:\n{success_msg}"
    );
    let warning = drain_next_system_message(&mut event_rx).await;
    assert!(
        warning.contains("exceeds")
            && warning.contains("default_max_num_ctx")
            && warning.contains("32 768"),
        "second message warns about the cap conflict; got:\n{warning}"
    );

    // The override still applies to the controller's effective
    // value — the warning is informational, not blocking.
    assert_eq!(
        controller.state.config.active_ollama_num_ctx_override(),
        Some(65_536)
    );
}

#[tokio::test]
async fn context_length_no_args_with_override_above_cap_includes_exceeds_marker() {
    // Cap PR 3 follow-on: after the user sets an above-cap
    // override and then queries (no args), the status message
    // must continue to flag the conflict. Otherwise the
    // warning lives only in the set-time scrollback and
    // disappears on next query.
    let mut runtime_state = RuntimeState::default();
    runtime_state
        .ollama_num_ctx_overrides
        .insert("ollama:qwen3:32b".into(), 65_536);
    let (_tempdir, mut controller, mut event_rx, _tx) =
        controller_for_context_length_test_with_cap(runtime_state, 32_768);

    controller
        .handle_action(UiAction::ContextLength(None))
        .await
        .expect("query context length");

    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(
        msg.contains("runtime override"),
        "must indicate override is active; got:\n{msg}"
    );
    assert!(
        msg.contains("exceeds"),
        "must surface the cap conflict on every query, not just at set time; got:\n{msg}"
    );
}

#[tokio::test]
async fn context_length_set_emits_status_update_with_effective_context_window() {
    let (_tempdir, mut controller, mut event_rx, _tx) =
        controller_for_context_length_test(RuntimeState::default());

    controller
        .handle_action(UiAction::ContextLength(Some("16384".into())))
        .await
        .expect("set context length");

    let event = event_rx.recv().await.expect("status update");
    assert!(matches!(
        event,
        AgentEvent::StatusUpdate {
            context_window: 16_384,
            ..
        }
    ));
}

#[tokio::test]
async fn context_length_override_persists_across_session_restart() {
    let tempdir = tempdir().expect("tempdir");
    let runtime_state_path = tempdir.path().join("state.json");
    let (mut controller, _event_rx, _tx) = build_dispatch_controller_with_runtime_state_path(
        vec![ollama_model()],
        16,
        RuntimeState::default(),
        Some(runtime_state_path.clone()),
    );

    controller
        .handle_action(UiAction::ContextLength(Some("16384".into())))
        .await
        .expect("set context length");

    let loaded = load_runtime_state_from(&runtime_state_path).expect("load state");
    assert_eq!(
        loaded
            .ollama_num_ctx_overrides
            .get("ollama:qwen3:32b")
            .copied(),
        Some(16_384)
    );
    let restarted = ConfigState::new(
        AnieConfig::default(),
        loaded,
        ollama_model(),
        ThinkingLevel::Medium,
        None,
    );
    assert_eq!(restarted.active_ollama_num_ctx_override(), Some(16_384));
}

#[tokio::test]
async fn context_length_override_applies_to_next_request_without_reload() {
    let tempdir = tempdir().expect("tempdir");
    let recorded_options = Arc::new(Mutex::new(Vec::new()));
    let mut provider_registry = ProviderRegistry::new();
    provider_registry.register(
        ApiKind::OllamaChatApi,
        Box::new(RecordingProvider {
            options: Arc::clone(&recorded_options),
        }),
    );
    let mut state =
        build_state_with_registry(ollama_model(), RuntimeState::default(), provider_registry);
    state
        .config
        .set_runtime_state_path_for_test(tempdir.path().join("state.json"));
    let (event_tx, _event_rx) = mpsc::channel(16);
    let (_ui_tx, ui_rx) = mpsc::unbounded_channel();
    let mut controller = InteractiveController::new(state, ui_rx, event_tx, false);

    controller
        .handle_action(UiAction::ContextLength(Some("16384".into())))
        .await
        .expect("set context length");
    let agent = build_agent(&controller.state, None, RlmExtras::empty());
    let (event_tx, _event_rx) = mpsc::channel(16);
    let result = agent
        .run(
            vec![user_message("hello")],
            Vec::new(),
            event_tx,
            CancellationToken::new(),
        )
        .await;

    assert!(result.terminal_error.is_none());
    let options = recorded_options.lock().expect("recorded options");
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].num_ctx_override, Some(16_384));
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
        compaction_stats: Arc::new(crate::compaction_stats::CompactionStatsAtomic::default()),
        harness_mode: crate::harness_mode::HarnessMode::default(),
        rlm_archived_messages: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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

#[tokio::test]
async fn model_change_with_runtime_persistence_failure_warns_but_updates_state() {
    let tempdir = tempdir().expect("tempdir");
    let unwritable_state_path = tempdir.path().join("state-directory");
    fs::create_dir_all(&unwritable_state_path).expect("create state directory");
    let (mut controller, mut event_rx) = controller_with_runtime_state_path(unwritable_state_path);

    controller
        .handle_action(UiAction::SetModel("gpt-4.1".into()))
        .await
        .expect("set model");

    assert_eq!(controller.state.config.current_model().id, "gpt-4.1");
    let messages = drain_system_messages(&mut event_rx);
    assert!(
        messages
            .iter()
            .any(|message| message.contains("setting is active for this session")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("may revert after restart")),
        "{messages:?}"
    );
}

#[tokio::test]
async fn thinking_change_with_runtime_persistence_failure_warns_but_updates_state() {
    let tempdir = tempdir().expect("tempdir");
    let unwritable_state_path = tempdir.path().join("state-directory");
    fs::create_dir_all(&unwritable_state_path).expect("create state directory");
    let (mut controller, mut event_rx) = controller_with_runtime_state_path(unwritable_state_path);

    controller
        .handle_action(UiAction::SetThinking("high".into()))
        .await
        .expect("set thinking");

    assert_eq!(
        controller.state.config.current_thinking(),
        ThinkingLevel::High
    );
    let messages = drain_system_messages(&mut event_rx);
    assert!(
        messages
            .iter()
            .any(|message| message.contains("setting is active for this session")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("may revert after restart")),
        "{messages:?}"
    );
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
        compaction_stats: Arc::new(crate::compaction_stats::CompactionStatsAtomic::default()),
        harness_mode: crate::harness_mode::HarnessMode::default(),
        rlm_archived_messages: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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

// ---------------------------------------------------------------
// /state command tests.
// ---------------------------------------------------------------

#[test]
fn state_summary_for_ollama_with_runtime_override_shows_all_three_layers() {
    let mut model = ollama_model();
    model.context_window = 32_768;

    let summary = format_state_summary(
        &model,
        ThinkingLevel::Medium,
        Some(16_384),
        Some(32_768),
        16_384,
        "session-abc",
        Some(std::path::PathBuf::from("/tmp/anie/config.toml")),
        Some(std::path::PathBuf::from("/tmp/anie/state.json")),
        crate::compaction_stats::CompactionStats::default(),
    );

    assert!(summary.contains("ollama:qwen3:32b"), "{summary}");
    assert!(
        summary.contains("Effective:        16 384 tokens (runtime override active)"),
        "{summary}",
    );
    assert!(
        summary.contains("Runtime override: 16 384 (state.json)"),
        "{summary}",
    );
    assert!(
        summary.contains("Workspace cap:    32 768 (config.toml [ollama] default_max_num_ctx)"),
        "{summary}",
    );
    assert!(
        summary.contains("Model baseline:   32 768 (Model.context_window)"),
        "{summary}",
    );
}

#[test]
fn state_summary_for_ollama_without_override_marks_override_none() {
    let summary = format_state_summary(
        &ollama_model(),
        ThinkingLevel::Off,
        None,
        Some(32_768),
        32_768,
        "session-1",
        None,
        None,
        crate::compaction_stats::CompactionStats::default(),
    );

    assert!(
        summary.contains("Effective:        32 768 tokens\n"),
        "header should not claim override active: {summary}",
    );
    assert!(summary.contains("Runtime override: (none)"), "{summary}");
    assert!(summary.contains("Workspace cap:    32 768"), "{summary}");
}

#[test]
fn state_summary_for_ollama_without_cap_marks_cap_none() {
    let mut model = ollama_model();
    model.context_window = 131_072;

    let summary = format_state_summary(
        &model,
        ThinkingLevel::High,
        None,
        None,
        131_072,
        "session-1",
        None,
        None,
        crate::compaction_stats::CompactionStats::default(),
    );

    assert!(summary.contains("Runtime override: (none)"), "{summary}");
    assert!(summary.contains("Workspace cap:    (none)"), "{summary}");
    assert!(summary.contains("Model baseline:   131 072"), "{summary}");
}

#[test]
fn state_summary_for_non_ollama_model_omits_layered_breakdown() {
    let summary = format_state_summary(
        &model("gpt-5", "openai"),
        ThinkingLevel::Medium,
        None,
        None,
        200_000,
        "session-x",
        None,
        None,
        crate::compaction_stats::CompactionStats::default(),
    );

    assert!(summary.contains("openai:gpt-5"), "{summary}");
    assert!(summary.contains("OpenAICompletions"), "{summary}");
    assert!(summary.contains("Effective: 200 000 tokens"), "{summary}");
    assert!(
        summary.contains("only apply to Ollama /api/chat models"),
        "{summary}",
    );
    assert!(
        !summary.contains("Runtime override:"),
        "non-Ollama model should not emit override row: {summary}",
    );
    assert!(
        !summary.contains("Workspace cap:"),
        "non-Ollama model should not emit cap row: {summary}",
    );
}

#[test]
fn state_summary_includes_thinking_and_session_id() {
    let summary = format_state_summary(
        &ollama_model(),
        ThinkingLevel::High,
        None,
        None,
        32_768,
        "abc-123-def",
        None,
        None,
        crate::compaction_stats::CompactionStats::default(),
    );

    assert!(summary.contains("Thinking: high"), "{summary}");
    assert!(summary.contains("Active: abc-123-def"), "{summary}");
}

#[test]
fn state_summary_lists_persistent_file_paths_when_available() {
    let summary = format_state_summary(
        &ollama_model(),
        ThinkingLevel::Off,
        None,
        None,
        32_768,
        "s",
        Some(std::path::PathBuf::from("/home/u/.anie/config.toml")),
        Some(std::path::PathBuf::from("/home/u/.anie/state.json")),
        crate::compaction_stats::CompactionStats::default(),
    );

    assert!(
        summary.contains("Config: /home/u/.anie/config.toml (hand-edited)"),
        "{summary}",
    );
    assert!(
        summary.contains("State:  /home/u/.anie/state.json (written by anie)"),
        "{summary}",
    );
}

#[test]
fn state_summary_omits_path_lines_when_path_helpers_return_none() {
    // Guards the no-HOME case where anie_dir() returns None.
    let summary = format_state_summary(
        &ollama_model(),
        ThinkingLevel::Off,
        None,
        None,
        32_768,
        "s",
        None,
        None,
        crate::compaction_stats::CompactionStats::default(),
    );

    assert!(summary.contains("Files"), "{summary}");
    assert!(!summary.contains("Config:"), "{summary}");
    assert!(!summary.contains("State: "), "{summary}");
}

#[tokio::test]
async fn show_state_action_emits_summary_as_system_message() {
    let (_tempdir, mut controller, mut event_rx, _tx) =
        controller_for_context_length_test_with_cap(RuntimeState::default(), 32_768);

    controller
        .handle_action(UiAction::ShowState)
        .await
        .expect("show state");

    let msg = drain_next_system_message(&mut event_rx).await;
    assert!(msg.starts_with("Current model"), "{msg}");
    assert!(msg.contains("ollama:qwen3:32b"), "{msg}");
    assert!(msg.contains("Workspace cap:    32 768"), "{msg}");
    assert!(msg.contains("Files"), "{msg}");
}
