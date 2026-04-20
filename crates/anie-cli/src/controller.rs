use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use time::{OffsetDateTime, format_description::FormatItem, macros::format_description};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::info;

use anie_agent::{AgentLoop, AgentLoopConfig, ToolExecutionMode, ToolRegistry};
use anie_auth::AuthResolver;
use anie_config::{AnieConfig, collect_context_files};
use anie_protocol::{AgentEvent, ContentBlock, Message, UserMessage, now_millis};
use anie_provider::{
    Model, ProviderError, ProviderRegistry, RequestOptionsResolver, ThinkingLevel,
};
use anie_session::{CompactionConfig, SessionContext, SessionInfo};
use anie_tui::UiAction;

use crate::{
    Cli,
    compaction::CompactionStrategy,
    model_catalog::{resolve_requested_model, upsert_model},
    runtime::{ConfigState, SessionHandle, SystemPromptCache},
    user_error::{HandleError, UserCommandError},
};

const DATE_FORMAT: &[FormatItem<'static>] = format_description!("[year]-[month]-[day]");

use crate::retry_policy::{RetryConfig, RetryDecision, RetryPolicy};

/// Start one-shot print mode.
pub async fn run_print_mode(cli: Cli) -> Result<()> {
    crate::print_mode::run_print_mode(cli).await
}

/// Start minimal JSONL RPC mode.
pub async fn run_rpc_mode(cli: Cli) -> Result<()> {
    crate::rpc::run_rpc_mode(cli).await
}

pub(crate) struct InteractiveController {
    state: ControllerState,
    ui_action_rx: mpsc::Receiver<UiAction>,
    event_tx: mpsc::Sender<AgentEvent>,
    current_run: Option<CurrentRun>,
    quitting: bool,
    exit_after_run: bool,
}

struct CurrentRun {
    handle: JoinHandle<anie_agent::AgentRunResult>,
    cancel: CancellationToken,
    already_compacted: bool,
    retry_attempt: u32,
}

impl InteractiveController {
    pub(crate) fn new(
        state: ControllerState,
        ui_action_rx: mpsc::Receiver<UiAction>,
        event_tx: mpsc::Sender<AgentEvent>,
        exit_after_run: bool,
    ) -> Self {
        Self {
            state,
            ui_action_rx,
            event_tx,
            current_run: None,
            quitting: false,
            exit_after_run,
        }
    }

    pub(crate) async fn run(mut self) -> Result<()> {
        anie_agent::send_event(&self.event_tx, self.state.status_event()).await;
        let _ = self
            .event_tx
            .send(AgentEvent::SystemMessage {
                text: format!("Session: {}", self.state.session.id()),
            })
            .await;

        loop {
            if let Some(current_run) = &mut self.current_run {
                tokio::select! {
                    maybe_action = self.ui_action_rx.recv() => {
                        match maybe_action {
                            Some(action) => self.handle_action(action).await?,
                            None => {
                                self.quitting = true;
                                current_run.cancel.cancel();
                            }
                        }
                    }
                    run_result = &mut current_run.handle => {
                        let already_compacted = current_run.already_compacted;
                        let retry_attempt = current_run.retry_attempt;
                        self.current_run = None;
                        match run_result {
                            Ok(result) => {
                                if let Some(error) = result.terminal_error.as_ref() {
                                    let policy = RetryPolicy {
                                        config: &self.state.retry_config,
                                    };
                                    match policy.decide(error, retry_attempt, already_compacted) {
                                        RetryDecision::Compact => {
                                            match self.state.retry_after_overflow(&self.event_tx).await {
                                                Ok(true) => {
                                                    self.start_continuation_run(true, retry_attempt).await?;
                                                }
                                                Ok(false) => {
                                                    self.state.finish_run(&result).await?;
                                                }
                                                Err(compaction_error) => {
                                                    anie_agent::send_event(&self.event_tx, AgentEvent::SystemMessage {
                                                        text: format!("Overflow recovery failed: {compaction_error}"),
                                                    }).await;
                                                    self.state.finish_run(&result).await?;
                                                }
                                            }
                                        }
                                        RetryDecision::Retry { attempt, delay_ms } => {
                                            self.state
                                                .schedule_transient_retry_with_delay(
                                                    &self.event_tx,
                                                    error,
                                                    attempt,
                                                    delay_ms,
                                                )
                                                .await?;
                                            self.start_continuation_run(already_compacted, attempt)
                                                .await?;
                                        }
                                        RetryDecision::GiveUp { reason } => {
                                            info!(?reason, retry_attempt, error = %error, "not retrying provider error");
                                            self.state.finish_run(&result).await?;
                                        }
                                    }
                                } else {
                                    self.state.finish_run(&result).await?;
                                }
                            }
                            Err(error) => {
                                anie_agent::send_event(&self.event_tx, AgentEvent::SystemMessage {
                                    text: format!("Agent task failed: {error}"),
                                }).await;
                            }
                        }
                        if self.exit_after_run && self.current_run.is_none() {
                            self.quitting = true;
                        }
                    }
                }
            } else {
                match self.ui_action_rx.recv().await {
                    Some(action) => self.handle_action(action).await?,
                    None => break,
                }
            }

            if self.quitting && self.current_run.is_none() {
                break;
            }
        }

        self.state.session.flush()?;
        Ok(())
    }

    /// Dispatch a `UiAction` and classify any resulting error.
    ///
    /// User-input errors (unknown model, invalid thinking level,
    /// unknown session) surface as system messages and return
    /// `Ok(())`. Anything else propagates and terminates the run
    /// loop, as before.
    ///
    /// Keeping classification in the wrapper — rather than
    /// inlining it at each call site — means every new slash
    /// command that funnels through `UiAction` gets the same
    /// containment for free.
    async fn handle_action(&mut self, action: UiAction) -> Result<()> {
        match self.try_handle_action(action).await {
            Ok(()) => Ok(()),
            Err(HandleError::User(user_err)) => {
                self.send_system_message(&user_err.to_string()).await;
                Ok(())
            }
            Err(HandleError::Fatal(error)) => Err(error),
        }
    }

    async fn try_handle_action(&mut self, action: UiAction) -> Result<(), HandleError> {
        match action {
            UiAction::SubmitPrompt(text) => {
                if self.current_run.is_some() {
                    let _ = self
                        .event_tx
                        .send(AgentEvent::SystemMessage {
                            text: "A run is already active. Press Ctrl+C to abort it first.".into(),
                        })
                        .await;
                } else {
                    self.start_prompt_run(text).await?;
                }
            }
            UiAction::Abort => {
                if let Some(current_run) = &self.current_run {
                    current_run.cancel.cancel();
                }
            }
            UiAction::Quit => {
                self.quitting = true;
                if let Some(current_run) = &self.current_run {
                    current_run.cancel.cancel();
                }
            }
            UiAction::SetModel(requested) => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot change models while a run is active.")
                        .await;
                } else {
                    self.state.set_model(&requested).await?;
                    anie_agent::send_event(&self.event_tx, self.state.status_event()).await;
                    self.send_system_message(&format!(
                        "Model set to {}:{}",
                        self.state.config.current_model().provider,
                        self.state.config.current_model().id,
                    ))
                    .await;
                }
            }
            UiAction::SetResolvedModel(model) => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot change models while a run is active.")
                        .await;
                } else {
                    self.state.set_model_resolved(model).await?;
                    anie_agent::send_event(&self.event_tx, self.state.status_event()).await;
                }
            }
            UiAction::SetThinking(level) => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot change thinking while a run is active.")
                        .await;
                } else {
                    self.state.set_thinking(&level).await?;
                    anie_agent::send_event(&self.event_tx, self.state.status_event()).await;
                    self.send_system_message(&format!(
                        "Thinking level set to {}",
                        format_thinking(self.state.config.current_thinking()),
                    ))
                    .await;
                }
            }
            UiAction::Compact => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot compact while a run is active.")
                        .await;
                } else {
                    self.state.force_compact(&self.event_tx).await?;
                }
            }
            UiAction::ForkSession => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot fork while a run is active.")
                        .await;
                } else {
                    let new_session_id = self.state.fork_session().await?;
                    let transcript = self
                        .state
                        .session_context()
                        .messages
                        .into_iter()
                        .map(|message| message.message)
                        .collect::<Vec<_>>();
                    let _ = self
                        .event_tx
                        .send(AgentEvent::TranscriptReplace {
                            messages: transcript,
                        })
                        .await;
                    anie_agent::send_event(&self.event_tx, self.state.status_event()).await;
                    self.send_system_message(&format!(
                        "Forked into child session {new_session_id}"
                    ))
                    .await;
                }
            }
            UiAction::ShowDiff => {
                self.send_system_message(&self.state.session_diff()).await;
            }
            UiAction::NewSession => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot start a new session while a run is active.")
                        .await;
                } else {
                    self.state.new_session().await?;
                    let _ = self
                        .event_tx
                        .send(AgentEvent::TranscriptReplace {
                            messages: Vec::new(),
                        })
                        .await;
                    anie_agent::send_event(&self.event_tx, self.state.status_event()).await;
                    self.send_system_message(&format!(
                        "Started new session {}",
                        self.state.session.id()
                    ))
                    .await;
                }
            }
            UiAction::ListSessions => {
                let sessions = self.state.list_sessions()?;
                self.send_system_message(&format_sessions(&sessions, self.state.session.id()))
                    .await;
            }
            UiAction::SwitchSession(session_id) => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot switch sessions while a run is active.")
                        .await;
                } else {
                    self.state.switch_session(&session_id).await?;
                    let transcript = self
                        .state
                        .session_context()
                        .messages
                        .into_iter()
                        .map(|message| message.message)
                        .collect::<Vec<_>>();
                    let _ = self
                        .event_tx
                        .send(AgentEvent::TranscriptReplace {
                            messages: transcript,
                        })
                        .await;
                    anie_agent::send_event(&self.event_tx, self.state.status_event()).await;
                    let _ = self
                        .event_tx
                        .send(AgentEvent::SystemMessage {
                            text: format!("Switched to session {session_id}"),
                        })
                        .await;
                }
            }
            UiAction::ShowTools => {
                let tools = self.state.tool_registry.definitions();
                let body = if tools.is_empty() {
                    "No tools are currently registered.".to_string()
                } else {
                    tools
                        .into_iter()
                        .map(|tool| format!("- {}: {}", tool.name, tool.description))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.send_system_message(&body).await;
            }
            UiAction::ShowHelp => {
                let help = self.state.command_registry.format_help();
                self.send_system_message(&help).await;
            }
            UiAction::GetState => {
                anie_agent::send_event(&self.event_tx, self.state.status_event()).await;
                self.send_system_message(&format!(
                    "Session: {}\nProvider: {}\nModel: {}\nThinking: {}",
                    self.state.session.id(),
                    self.state.config.current_model().provider,
                    self.state.config.current_model().id,
                    format_thinking(self.state.config.current_thinking()),
                ))
                .await;
            }
            UiAction::SelectModel => {
                self.send_system_message("Use /model <id> to switch models.")
                    .await;
            }
            UiAction::ReloadConfig { provider, model } => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot reload config while a run is active.")
                        .await;
                } else {
                    self.state
                        .reload_config(provider.as_deref(), model.as_deref())
                        .await?;
                    anie_agent::send_event(&self.event_tx, self.state.status_event()).await;
                    self.send_system_message("Configuration reloaded.").await;
                }
            }
            UiAction::ClearOutput => {}
        }
        Ok(())
    }

    async fn start_prompt_run(&mut self, text: String) -> Result<()> {
        info!(
            provider = %self.state.config.current_model().provider,
            model = %self.state.config.current_model().id,
            thinking = %format_thinking(self.state.config.current_thinking()),
            "starting interactive run"
        );
        self.state.refresh_system_prompt_if_needed();
        let prompt_message = Message::User(UserMessage {
            content: vec![ContentBlock::Text { text }],
            timestamp: now_millis(),
        });

        let prompt_entry_id = self
            .state
            .session
            .inner_mut()
            .append_message(&prompt_message)?;
        if self.state.config.anie_config().compaction.enabled {
            self.state.maybe_auto_compact(&self.event_tx).await?;
        }
        let context = self.state.context_without_entry(Some(&prompt_entry_id));
        let agent = build_agent(&self.state);
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let event_tx = self.event_tx.clone();
        let handle = tokio::spawn(async move {
            agent
                .run(vec![prompt_message], context, event_tx, task_cancel)
                .await
        });
        self.current_run = Some(CurrentRun {
            handle,
            cancel,
            already_compacted: false,
            retry_attempt: 0,
        });
        Ok(())
    }

    async fn start_continuation_run(
        &mut self,
        already_compacted: bool,
        retry_attempt: u32,
    ) -> Result<()> {
        self.state.refresh_system_prompt_if_needed();
        let context = self
            .state
            .session_context()
            .messages
            .into_iter()
            .map(|message| message.message)
            .collect::<Vec<_>>();
        let agent = build_agent(&self.state);
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let event_tx = self.event_tx.clone();
        let handle =
            tokio::spawn(
                async move { agent.run(Vec::new(), context, event_tx, task_cancel).await },
            );
        self.current_run = Some(CurrentRun {
            handle,
            cancel,
            already_compacted,
            retry_attempt,
        });
        Ok(())
    }

    async fn send_system_message(&self, text: &str) {
        let _ = self
            .event_tx
            .send(AgentEvent::SystemMessage {
                text: text.to_string(),
            })
            .await;
    }
}

/// Shared state for the interactive controller.
///
/// Composed of focused handles:
/// - `session: SessionHandle` — current session plus fork/switch helpers.
/// - `config: ConfigState` — config.toml + runtime-state + current
///   model/thinking selections.
/// - `model_catalog: Vec<Model>` — known models across providers.
/// - `provider_registry: Arc<ProviderRegistry>` — active providers.
/// - `tool_registry: Arc<ToolRegistry>` — active tools.
/// - `request_options_resolver: Arc<dyn RequestOptionsResolver>` —
///   per-request auth/options resolution.
/// - `prompt_cache: SystemPromptCache` — cached system prompt and
///   context-file stamp.
/// - `retry_config: RetryConfig` — retry knobs.
/// - `command_registry: CommandRegistry` — builtin + future extension
///   slash-command metadata.
///
/// Methods on this struct are delegators to one of the handles or
/// event-emission coordinators. Long stateful logic belongs in a
/// dedicated helper/module, not here.
pub(crate) struct ControllerState {
    pub(crate) config: ConfigState,
    pub(crate) session: SessionHandle,
    pub(crate) model_catalog: Vec<Model>,
    pub(crate) provider_registry: Arc<ProviderRegistry>,
    pub(crate) tool_registry: Arc<ToolRegistry>,
    pub(crate) request_options_resolver: Arc<dyn RequestOptionsResolver>,
    pub(crate) prompt_cache: SystemPromptCache,
    pub(crate) retry_config: RetryConfig,
    /// Catalog of registered slash commands. Sourced from
    /// `commands::builtin_commands()` at startup; extensions and
    /// prompt templates register additional entries here.
    pub(crate) command_registry: crate::commands::CommandRegistry,
}

impl ControllerState {
    pub(crate) fn persist_runtime_state(&mut self) {
        self.config.persist_runtime_state(self.session.id());
    }

    async fn set_model(&mut self, requested: &str) -> Result<()> {
        let model = resolve_requested_model(
            requested,
            &self.config.current_model().provider,
            &self.model_catalog,
        )
        .map_err(|_| UserCommandError::UnknownModel(requested.to_string()))?;
        self.set_model_resolved(model).await
    }

    async fn set_model_resolved(&mut self, model: Model) -> Result<()> {
        upsert_model(&mut self.model_catalog, &model);
        self.config.set_model(model);
        self.session.inner_mut().append_model_change(
            &self.config.current_model().provider,
            &self.config.current_model().id,
        )?;
        self.config.persist_runtime_state(self.session.id());
        Ok(())
    }

    async fn set_thinking(&mut self, requested: &str) -> Result<()> {
        let level = parse_thinking_level(requested)
            .map_err(|_| UserCommandError::InvalidThinkingLevel(requested.to_string()))?;
        self.config.set_thinking(level);
        self.session
            .inner_mut()
            .append_thinking_change(self.config.current_thinking())?;
        self.config.persist_runtime_state(self.session.id());
        Ok(())
    }

    /// Build the compaction config + summarizer for the current
    /// session state. Used by every compaction call site.
    fn compaction_strategy(
        &self,
        keep_recent_tokens: u64,
    ) -> (CompactionConfig, CompactionStrategy) {
        let config = CompactionConfig {
            context_window: self.config.current_model().context_window,
            reserve_tokens: self.config.anie_config().compaction.reserve_tokens,
            keep_recent_tokens,
        };
        let strategy = CompactionStrategy::new(
            self.config.current_model().clone(),
            Arc::clone(&self.provider_registry),
            Arc::clone(&self.request_options_resolver),
        );
        (config, strategy)
    }

    /// Emit the `CompactionEnd` event for a successful compaction.
    /// Callers decide whether to follow with a status refresh or a
    /// transcript replacement, since the ordering matters visually.
    async fn emit_compaction_end(
        &self,
        event_tx: &mpsc::Sender<AgentEvent>,
        result: &anie_session::CompactionResult,
    ) {
        let tokens_after = self.estimated_context_tokens();
        anie_agent::send_event(
            event_tx,
            AgentEvent::CompactionEnd {
                summary: result.summary.clone(),
                tokens_before: result.tokens_before,
                tokens_after,
            },
        )
        .await;
    }

    async fn maybe_auto_compact(&mut self, event_tx: &mpsc::Sender<AgentEvent>) -> Result<()> {
        let (config, strategy) =
            self.compaction_strategy(self.config.anie_config().compaction.keep_recent_tokens);
        if let Some(result) = self
            .session
            .inner_mut()
            .auto_compact(&config, &strategy)
            .await?
        {
            anie_agent::send_event(event_tx, AgentEvent::CompactionStart).await;
            self.emit_compaction_end(event_tx, &result).await;
            anie_agent::send_event(event_tx, self.status_event()).await;
        }
        Ok(())
    }

    async fn force_compact(&mut self, event_tx: &mpsc::Sender<AgentEvent>) -> Result<()> {
        let (config, strategy) =
            self.compaction_strategy(self.config.anie_config().compaction.keep_recent_tokens);
        anie_agent::send_event(event_tx, AgentEvent::CompactionStart).await;
        match self
            .session
            .inner_mut()
            .force_compact(&config, &strategy)
            .await?
        {
            Some(result) => {
                self.emit_compaction_end(event_tx, &result).await;
                anie_agent::send_event(event_tx, self.status_event()).await;
            }
            None => {
                anie_agent::send_event(
                    event_tx,
                    AgentEvent::SystemMessage {
                        text: "Nothing to compact yet.".into(),
                    },
                )
                .await;
            }
        }
        Ok(())
    }

    async fn new_session(&mut self) -> Result<()> {
        self.session.start_new()?;
        self.config.persist_runtime_state(self.session.id());
        Ok(())
    }

    async fn switch_session(&mut self, session_id: &str) -> Result<()> {
        self.session
            .switch_to(session_id)
            .map_err(|_| UserCommandError::UnknownSession(session_id.to_string()))?;
        self.apply_session_overrides();
        self.config.persist_runtime_state(self.session.id());
        Ok(())
    }

    async fn fork_session(&mut self) -> Result<String> {
        let child_id = self.session.fork()?;
        self.apply_session_overrides();
        self.config.persist_runtime_state(self.session.id());
        Ok(child_id)
    }

    async fn finish_run(&mut self, result: &anie_agent::AgentRunResult) -> Result<()> {
        info!(
            generated_messages = result.generated_messages.len(),
            provider = %self.config.current_model().provider,
            model = %self.config.current_model().id,
            "persisting completed run"
        );
        self.session
            .inner_mut()
            .append_messages(&result.generated_messages)?;
        Ok(())
    }

    async fn schedule_transient_retry_with_delay(
        &mut self,
        event_tx: &mpsc::Sender<AgentEvent>,
        error: &ProviderError,
        retry_attempt: u32,
        delay_ms: u64,
    ) -> Result<()> {
        anie_agent::send_event(
            event_tx,
            AgentEvent::RetryScheduled {
                attempt: retry_attempt,
                max_retries: self.retry_config.max_retries,
                delay_ms,
                error: error.to_string(),
            },
        )
        .await;
        self.emit_transcript_replace_and_status(event_tx).await;
        info!(retry_attempt, delay_ms, error = %error, "scheduling transient provider retry");
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        Ok(())
    }

    async fn retry_after_overflow(&mut self, event_tx: &mpsc::Sender<AgentEvent>) -> Result<bool> {
        anie_agent::send_event(
            event_tx,
            AgentEvent::SystemMessage {
                text: "Context window exceeded; compacting and retrying...".into(),
            },
        )
        .await;
        // Overflow recovery halves the keep-recent budget — we're already
        // over the context window, so we need to discard more aggressively.
        let keep_recent = (self.config.anie_config().compaction.keep_recent_tokens / 2).max(1_000);
        let (config, strategy) = self.compaction_strategy(keep_recent);
        anie_agent::send_event(event_tx, AgentEvent::CompactionStart).await;
        match self
            .session
            .inner_mut()
            .force_compact(&config, &strategy)
            .await?
        {
            Some(result) => {
                self.emit_compaction_end(event_tx, &result).await;
                self.emit_transcript_replace_and_status(event_tx).await;
                Ok(true)
            }
            None => {
                anie_agent::send_event(
                    event_tx,
                    AgentEvent::SystemMessage {
                        text: "Context overflow recovery could not compact the session further."
                            .into(),
                    },
                )
                .await;
                Ok(false)
            }
        }
    }

    fn session_diff(&self) -> String {
        self.session.diff()
    }

    pub(crate) fn session_context(&self) -> SessionContext {
        self.session.context()
    }

    fn context_without_entry(&self, entry_id: Option<&str>) -> Vec<Message> {
        self.session.context_without_entry(entry_id)
    }

    fn estimated_context_tokens(&self) -> u64 {
        self.session.estimated_context_tokens()
    }

    fn transcript_messages(&self) -> Vec<Message> {
        self.session_context()
            .messages
            .into_iter()
            .map(|message| message.message)
            .collect()
    }

    async fn emit_transcript_replace_and_status(&self, event_tx: &mpsc::Sender<AgentEvent>) {
        anie_agent::send_event(
            event_tx,
            AgentEvent::TranscriptReplace {
                messages: self.transcript_messages(),
            },
        )
        .await;
        anie_agent::send_event(event_tx, self.status_event()).await;
    }

    pub(crate) fn status_event(&self) -> AgentEvent {
        AgentEvent::StatusUpdate {
            provider: self.config.current_model().provider.clone(),
            model_name: self.config.current_model().id.clone(),
            thinking: format_thinking(self.config.current_thinking()),
            estimated_context_tokens: self.estimated_context_tokens(),
            context_window: self.config.current_model().context_window,
            cwd: self.session.cwd().display().to_string(),
            session_id: self.session.id().to_string(),
        }
    }

    pub(crate) fn model_catalog(&self) -> &[Model] {
        &self.model_catalog
    }

    fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        self.session.list()
    }

    pub(crate) fn apply_session_overrides(&mut self) {
        let context = self.session.context();
        self.config
            .apply_session_overrides(&context, &mut self.model_catalog);
    }

    async fn reload_config(
        &mut self,
        requested_provider: Option<&str>,
        requested_model: Option<&str>,
    ) -> Result<()> {
        let outcome = self
            .config
            .reload_from_disk(requested_provider, requested_model)
            .await?;
        self.model_catalog = outcome.model_catalog;
        self.config.set_model(outcome.current_model);
        self.config.set_thinking(outcome.current_thinking);
        self.request_options_resolver = Arc::new(AuthResolver::new(
            self.config.cli_api_key().map(str::to_string),
            self.config.anie_config().clone(),
        ));
        let cwd = self.session.cwd().to_path_buf();
        self.prompt_cache
            .replace(&cwd, &self.tool_registry, self.config.anie_config())?;
        self.config.persist_runtime_state(self.session.id());
        Ok(())
    }

    /// Rebuild the system prompt if the set of context files or any of their mtimes changed.
    fn refresh_system_prompt_if_needed(&mut self) {
        let cwd = self.session.cwd().to_path_buf();
        self.prompt_cache
            .refresh_if_stale(&cwd, &self.tool_registry, self.config.anie_config());
    }
}

fn build_agent(state: &ControllerState) -> AgentLoop {
    AgentLoop::new(
        Arc::clone(&state.provider_registry),
        Arc::clone(&state.tool_registry),
        AgentLoopConfig::new(
            state.config.current_model().clone(),
            state.prompt_cache.current().to_string(),
            state.config.current_thinking(),
            ToolExecutionMode::Parallel,
            Arc::clone(&state.request_options_resolver),
        ),
    )
}

/// Build the system prompt for interactive, print, and RPC runs.
pub fn build_system_prompt(
    cwd: &Path,
    tools: &ToolRegistry,
    config: &AnieConfig,
) -> Result<String> {
    let tool_list = tools
        .definitions()
        .into_iter()
        .map(|tool| format!("- {}: {}", tool.name, tool.description))
        .collect::<Vec<_>>()
        .join("\n");

    let default_base = if tool_list.is_empty() {
        "You are an expert coding assistant. Be concise in your responses.".to_string()
    } else {
        format!(
            "You are an expert coding assistant. You help users by reading files, executing commands, editing code, and writing new files.\n\nAvailable tools:\n{tool_list}\n\nGuidelines:\n- Use bash for file operations like ls, grep, find\n- Use read to examine files (use offset + limit for large files)\n- Use edit for precise changes\n- Use write only for new files or complete rewrites\n- Be concise in your responses"
        )
    };

    let mut parts = vec![default_base];
    for context_file in collect_context_files(cwd, &config.context)? {
        parts.push(format!(
            "# Project Context\n\n## {}\n\n{}",
            context_file.path.display(),
            context_file.contents,
        ));
    }
    parts.push(format!("Current date: {}", current_date_ymd()?));
    parts.push(format!("Current working directory: {}", cwd.display()));
    Ok(parts.join("\n\n"))
}

/// Return a deterministic stamp of the currently-visible context files and their mtimes.
///
/// Unlike a single max-mtime, this detects deletion or modification of any context file.
pub(crate) fn context_files_stamp(
    cwd: &Path,
    config: &AnieConfig,
) -> Vec<(PathBuf, Option<std::time::SystemTime>)> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();

    for directory in cwd.ancestors() {
        for filename in &config.context.filenames {
            let candidate = directory.join(filename);
            if !candidate.is_file() || !seen.insert(candidate.clone()) {
                continue;
            }
            let mtime = std::fs::metadata(&candidate)
                .and_then(|metadata| metadata.modified())
                .ok();
            files.push((candidate, mtime));
        }
    }

    files
}

fn current_date_ymd() -> Result<String> {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    now.format(DATE_FORMAT)
        .context("failed to format current date")
}

pub(crate) fn parse_thinking_level(value: &str) -> Result<ThinkingLevel, String> {
    match value.to_ascii_lowercase().as_str() {
        "off" => Ok(ThinkingLevel::Off),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        _ => Err(format!("invalid thinking level '{value}'")),
    }
}

fn format_thinking(level: ThinkingLevel) -> String {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
    }
    .to_string()
}

fn format_sessions(sessions: &[SessionInfo], current_session_id: &str) -> String {
    if sessions.is_empty() {
        return "No sessions found.".into();
    }

    sessions
        .iter()
        .map(|session| {
            format!(
                "{} {}  {}  {}",
                if session.id == current_session_id {
                    '*'
                } else {
                    ' '
                },
                session.id,
                session.cwd,
                truncate_text(&session.first_message, 60),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let truncated = text
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>();
        format!("{truncated}…")
    }
}

#[cfg(test)]
#[path = "controller_tests.rs"]
mod controller_tests;
