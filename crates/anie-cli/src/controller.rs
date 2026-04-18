use std::{
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::FormatItem, macros::format_description};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::mpsc,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use anie_agent::{AgentLoop, AgentLoopConfig, ToolExecutionMode, ToolRegistry};
use anie_auth::AuthResolver;
use anie_config::{AnieConfig, CliOverrides, collect_context_files, load_config};
use anie_protocol::{
    AgentEvent, ContentBlock, Message, StopReason, StreamDelta, UserMessage, now_millis,
};
use anie_provider::{
    Model, ProviderError, ProviderRegistry, RequestOptionsResolver, ThinkingLevel,
};
use anie_providers_builtin::register_builtin_providers;
use anie_session::{CompactionConfig, SessionContext, SessionInfo, SessionManager};
use anie_tools::{BashTool, EditTool, FileMutationQueue, ReadTool, WriteTool};
use anie_tui::{App, UiAction, install_panic_hook, restore_terminal, run_tui, setup_terminal};

use crate::{
    Cli,
    compaction::CompactionStrategy,
    model_catalog::{
        build_model_catalog, fallback_model_from_provider, resolve_initial_selection,
        resolve_model, resolve_requested_model, upsert_model,
    },
    runtime::{SessionHandle, SystemPromptCache},
    runtime_state::{RuntimeState, load_runtime_state, save_runtime_state},
};

const DATE_FORMAT: &[FormatItem<'static>] = format_description!("[year]-[month]-[day]");

use crate::retry_policy::{RetryConfig, retry_delay_ms};

/// Start the full interactive TUI mode.
pub async fn run_interactive_mode(cli: Cli) -> Result<()> {
    install_panic_hook();

    let state = prepare_controller_state(&cli).await?;
    let transcript = state
        .session_context()
        .messages
        .into_iter()
        .map(|message| message.message)
        .collect::<Vec<_>>();
    let initial_status = state.status_event();

    let (agent_event_tx, agent_event_rx) = mpsc::channel(256);
    let (ui_action_tx, ui_action_rx) = mpsc::channel(64);
    spawn_shutdown_signal_forwarder(ui_action_tx.clone());

    let initial_models = state.model_catalog.clone();
    let controller = InteractiveController::new(state, ui_action_rx, agent_event_tx, false);
    let controller_task = tokio::spawn(async move { controller.run().await });

    let mut app = App::new(agent_event_rx, ui_action_tx, initial_models);
    apply_status_event(app.status_bar_mut(), &initial_status);
    app.load_transcript(&transcript);

    let mut terminal = setup_terminal()?;
    let result = run_tui(&mut terminal, &mut app).await;
    restore_terminal(&mut terminal)?;

    match controller_task.await {
        Ok(controller_result) => controller_result?,
        Err(error) => return Err(anyhow!("interactive controller task failed: {error}")),
    }

    result
}

/// Start one-shot print mode.
pub async fn run_print_mode(cli: Cli) -> Result<()> {
    let prompt = cli.prompt.join(" ");
    if prompt.trim().is_empty() {
        anyhow::bail!("No prompt provided. Usage: anie 'your prompt here'");
    }

    let state = prepare_controller_state(&cli).await?;
    let (agent_event_tx, mut agent_event_rx) = mpsc::channel(256);
    let (ui_action_tx, ui_action_rx) = mpsc::channel(64);
    let controller = InteractiveController::new(state, ui_action_rx, agent_event_tx, true);
    let controller_task = tokio::spawn(async move { controller.run().await });

    let abort_tx = ui_action_tx.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = abort_tx.send(UiAction::Abort).await;
    });

    ui_action_tx
        .send(UiAction::SubmitPrompt(prompt))
        .await
        .context("failed to start print-mode prompt")?;

    let mut streamed_text = false;
    let mut printed_assistant_output = false;
    let mut pending_terminal_text: Option<String> = None;
    while let Some(event) = agent_event_rx.recv().await {
        match event {
            AgentEvent::MessageStart {
                message: Message::Assistant(_),
            } => {
                streamed_text = false;
                pending_terminal_text = None;
            }
            AgentEvent::MessageDelta {
                delta: StreamDelta::TextDelta(text),
            } => {
                print!("{text}");
                std::io::stdout()
                    .flush()
                    .context("failed to flush stdout")?;
                streamed_text = true;
                printed_assistant_output = true;
                pending_terminal_text = None;
            }
            AgentEvent::MessageEnd {
                message: Message::Assistant(assistant),
            } if !streamed_text => {
                let text = assistant_text(&assistant.content);
                if matches!(assistant.stop_reason, StopReason::Error) {
                    pending_terminal_text = Some(text);
                } else if !text.is_empty() {
                    print!("{text}");
                    std::io::stdout()
                        .flush()
                        .context("failed to flush stdout")?;
                    printed_assistant_output = true;
                }
            }
            AgentEvent::ToolExecStart {
                tool_name, args, ..
            } => {
                eprintln!("\n[tool: {tool_name} {}]", tool_hint(&args));
            }
            AgentEvent::ToolExecEnd { is_error, .. } if is_error => {
                eprintln!("[tool error]");
            }
            AgentEvent::SystemMessage { text } => {
                eprintln!("\n{text}");
            }
            AgentEvent::CompactionStart => {
                eprintln!("\n[compacting context]");
            }
            AgentEvent::CompactionEnd {
                tokens_before,
                tokens_after,
                ..
            } => {
                eprintln!(
                    "\n[compaction complete: {} -> {}]",
                    format_tokens(tokens_before),
                    format_tokens(tokens_after)
                );
            }
            AgentEvent::RetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => {
                pending_terminal_text = None;
                eprintln!(
                    "\n[retrying {}/{} in {:.1}s: {}]",
                    attempt,
                    max_retries,
                    delay_ms as f64 / 1000.0,
                    error,
                );
            }
            AgentEvent::TranscriptReplace { .. } => {
                pending_terminal_text = None;
                streamed_text = false;
            }
            AgentEvent::AgentEnd { messages } => {
                if !printed_assistant_output
                    && let Some(Message::Assistant(assistant)) = messages.last()
                    && !matches!(assistant.stop_reason, StopReason::Error)
                {
                    let text = assistant_text(&assistant.content);
                    if !text.is_empty() {
                        print!("{text}");
                        std::io::stdout()
                            .flush()
                            .context("failed to flush stdout")?;
                        printed_assistant_output = true;
                    }
                }
            }
            _ => {}
        }
    }

    if !printed_assistant_output
        && let Some(text) = pending_terminal_text
        && !text.is_empty()
    {
        print!("{text}");
        std::io::stdout()
            .flush()
            .context("failed to flush stdout")?;
    }
    println!();
    let _ = ui_action_tx.send(UiAction::Quit).await;
    match controller_task.await {
        Ok(result) => result,
        Err(error) => Err(anyhow!("print-mode controller task failed: {error}")),
    }
}

/// Start minimal JSONL RPC mode.
pub async fn run_rpc_mode(cli: Cli) -> Result<()> {
    let state = prepare_controller_state(&cli).await?;
    let (agent_event_tx, agent_event_rx) = mpsc::channel(256);
    let (ui_action_tx, ui_action_rx) = mpsc::channel(64);
    let controller = InteractiveController::new(state, ui_action_rx, agent_event_tx, false);

    let hello = serde_json::to_string(&RpcEvent::Hello { version: 1 })?;
    let mut stdout = BufWriter::new(tokio::io::stdout());
    stdout
        .write_all(hello.as_bytes())
        .await
        .context("failed to write RPC hello")?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;

    let controller_task = tokio::spawn(async move { controller.run().await });
    let printer_task = tokio::spawn(async move { rpc_event_printer(agent_event_rx).await });

    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    while let Some(line) = lines
        .next_line()
        .await
        .context("failed to read RPC input")?
    {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<RpcCommand>(&line) {
            Ok(command) => {
                let action = match command {
                    RpcCommand::Prompt { text } => UiAction::SubmitPrompt(text),
                    RpcCommand::Abort => UiAction::Abort,
                    RpcCommand::GetState => UiAction::GetState,
                    RpcCommand::SetModel { model, provider } => UiAction::SetModel(
                        provider
                            .map(|provider_name| format!("{provider_name}:{model}"))
                            .unwrap_or(model),
                    ),
                    RpcCommand::SetThinking { level } => UiAction::SetThinking(level),
                };
                if ui_action_tx.send(action).await.is_err() {
                    break;
                }
            }
            Err(error) => {
                write_rpc_error(&format!("invalid command: {error}")).await?;
            }
        }
    }

    drop(ui_action_tx);
    match controller_task.await {
        Ok(result) => result?,
        Err(error) => return Err(anyhow!("RPC controller task failed: {error}")),
    }
    match printer_task.await {
        Ok(result) => result?,
        Err(error) => return Err(anyhow!("RPC event printer task failed: {error}")),
    }
    Ok(())
}

struct InteractiveController {
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
    allow_overflow_retry: bool,
    retry_attempt: u32,
}

impl InteractiveController {
    fn new(
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

    async fn run(mut self) -> Result<()> {
        let _ = self.event_tx.send(self.state.status_event()).await;
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
                        let allow_overflow_retry = current_run.allow_overflow_retry;
                        let retry_attempt = current_run.retry_attempt;
                        self.current_run = None;
                        match run_result {
                            Ok(result) => {
                                if allow_overflow_retry
                                    && self.state.should_retry_after_overflow(&result)
                                {
                                    match self.state.retry_after_overflow(&self.event_tx).await {
                                        Ok(true) => {
                                            self.start_continuation_run(false, retry_attempt).await?;
                                        }
                                        Ok(false) => {
                                            self.state.finish_run(&result).await?;
                                        }
                                        Err(error) => {
                                            let _ = self.event_tx.send(AgentEvent::SystemMessage {
                                                text: format!("Overflow recovery failed: {error}"),
                                            }).await;
                                            self.state.finish_run(&result).await?;
                                        }
                                    }
                                } else if self.state.should_retry_transient(&result, retry_attempt) {
                                    if let Some(error) = result.terminal_error.as_ref() {
                                        self.state
                                            .schedule_transient_retry(&self.event_tx, error, retry_attempt + 1)
                                            .await?;
                                        self.start_continuation_run(allow_overflow_retry, retry_attempt + 1)
                                            .await?;
                                    } else {
                                        self.state.finish_run(&result).await?;
                                    }
                                } else {
                                    self.state.finish_run(&result).await?;
                                }
                            }
                            Err(error) => {
                                let _ = self.event_tx.send(AgentEvent::SystemMessage {
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

    async fn handle_action(&mut self, action: UiAction) -> Result<()> {
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
                    let _ = self.event_tx.send(self.state.status_event()).await;
                }
            }
            UiAction::SetResolvedModel(model) => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot change models while a run is active.")
                        .await;
                } else {
                    self.state.set_model_resolved(model).await?;
                    let _ = self.event_tx.send(self.state.status_event()).await;
                }
            }
            UiAction::SetThinking(level) => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot change thinking while a run is active.")
                        .await;
                } else {
                    self.state.set_thinking(&level).await?;
                    let _ = self.event_tx.send(self.state.status_event()).await;
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
                    let _ = self.event_tx.send(self.state.status_event()).await;
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
                    let _ = self.event_tx.send(self.state.status_event()).await;
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
                    let _ = self.event_tx.send(self.state.status_event()).await;
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
            UiAction::GetState => {
                let _ = self.event_tx.send(self.state.status_event()).await;
                self.send_system_message(&format!(
                    "Session: {}\nProvider: {}\nModel: {}\nThinking: {}",
                    self.state.session.id(),
                    self.state.current_model.provider,
                    self.state.current_model.id,
                    format_thinking(self.state.current_thinking),
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
                    let _ = self.event_tx.send(self.state.status_event()).await;
                    self.send_system_message("Configuration reloaded.").await;
                }
            }
            UiAction::ClearOutput => {}
        }
        Ok(())
    }

    async fn start_prompt_run(&mut self, text: String) -> Result<()> {
        info!(
            provider = %self.state.current_model.provider,
            model = %self.state.current_model.id,
            thinking = %format_thinking(self.state.current_thinking),
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
        if self.state.config.compaction.enabled {
            self.state.maybe_auto_compact(&self.event_tx).await?;
        }
        let context = self.state.context_without_entry(Some(&prompt_entry_id));
        let agent = self.state.build_agent();
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
            allow_overflow_retry: true,
            retry_attempt: 0,
        });
        Ok(())
    }

    async fn start_continuation_run(
        &mut self,
        allow_overflow_retry: bool,
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
        let agent = self.state.build_agent();
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
            allow_overflow_retry,
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

struct ControllerState {
    config: AnieConfig,
    cli_api_key: Option<String>,
    session: SessionHandle,
    current_model: Model,
    current_thinking: ThinkingLevel,
    model_catalog: Vec<Model>,
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: Arc<ToolRegistry>,
    request_options_resolver: Arc<dyn RequestOptionsResolver>,
    prompt_cache: SystemPromptCache,
    runtime_state: RuntimeState,
    retry_config: RetryConfig,
    /// Catalog of registered slash commands. Sourced from
    /// `commands::builtin_commands()` at startup; extensions and
    /// prompt templates register additional entries here.
    #[allow(dead_code)] // consumed by /help once wired up; see plan 03 phase 3 status
    command_registry: crate::commands::CommandRegistry,
}

impl ControllerState {
    async fn set_model(&mut self, requested: &str) -> Result<()> {
        let model =
            resolve_requested_model(requested, &self.current_model.provider, &self.model_catalog)?;
        self.set_model_resolved(model).await
    }

    async fn set_model_resolved(&mut self, model: Model) -> Result<()> {
        upsert_model(&mut self.model_catalog, &model);
        self.current_model = model;
        self.session
            .inner_mut()
            .append_model_change(&self.current_model.provider, &self.current_model.id)?;
        self.persist_runtime_state();
        Ok(())
    }

    async fn set_thinking(&mut self, requested: &str) -> Result<()> {
        self.current_thinking = parse_thinking_level(requested).map_err(|error| anyhow!(error))?;
        self.session
            .inner_mut()
            .append_thinking_change(self.current_thinking)?;
        self.persist_runtime_state();
        Ok(())
    }

    /// Build the compaction config + summarizer for the current
    /// session state. Used by every compaction call site.
    fn compaction_strategy(
        &self,
        keep_recent_tokens: u64,
    ) -> (CompactionConfig, CompactionStrategy) {
        let config = CompactionConfig {
            context_window: self.current_model.context_window,
            reserve_tokens: self.config.compaction.reserve_tokens,
            keep_recent_tokens,
        };
        let strategy = CompactionStrategy::new(
            self.current_model.clone(),
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
        let _ = event_tx
            .send(AgentEvent::CompactionEnd {
                summary: result.summary.clone(),
                tokens_before: result.tokens_before,
                tokens_after,
            })
            .await;
    }

    async fn maybe_auto_compact(&mut self, event_tx: &mpsc::Sender<AgentEvent>) -> Result<()> {
        let (config, strategy) =
            self.compaction_strategy(self.config.compaction.keep_recent_tokens);
        if let Some(result) = self
            .session
            .inner_mut()
            .auto_compact(&config, &strategy)
            .await?
        {
            let _ = event_tx.send(AgentEvent::CompactionStart).await;
            self.emit_compaction_end(event_tx, &result).await;
            let _ = event_tx.send(self.status_event()).await;
        }
        Ok(())
    }

    async fn force_compact(&mut self, event_tx: &mpsc::Sender<AgentEvent>) -> Result<()> {
        let (config, strategy) =
            self.compaction_strategy(self.config.compaction.keep_recent_tokens);
        let _ = event_tx.send(AgentEvent::CompactionStart).await;
        match self
            .session
            .inner_mut()
            .force_compact(&config, &strategy)
            .await?
        {
            Some(result) => {
                self.emit_compaction_end(event_tx, &result).await;
                let _ = event_tx.send(self.status_event()).await;
            }
            None => {
                let _ = event_tx
                    .send(AgentEvent::SystemMessage {
                        text: "Nothing to compact yet.".into(),
                    })
                    .await;
            }
        }
        Ok(())
    }

    async fn new_session(&mut self) -> Result<()> {
        self.session.start_new()?;
        self.persist_runtime_state();
        Ok(())
    }

    async fn switch_session(&mut self, session_id: &str) -> Result<()> {
        self.session.switch_to(session_id)?;
        self.apply_session_overrides();
        self.persist_runtime_state();
        Ok(())
    }

    async fn fork_session(&mut self) -> Result<String> {
        let child_id = self.session.fork()?;
        self.apply_session_overrides();
        self.persist_runtime_state();
        Ok(child_id)
    }

    async fn finish_run(&mut self, result: &anie_agent::AgentRunResult) -> Result<()> {
        info!(
            generated_messages = result.generated_messages.len(),
            provider = %self.current_model.provider,
            model = %self.current_model.id,
            "persisting completed run"
        );
        self.session
            .inner_mut()
            .append_messages(&result.generated_messages)?;
        Ok(())
    }

    fn should_retry_after_overflow(&self, result: &anie_agent::AgentRunResult) -> bool {
        matches!(
            result.terminal_error.as_ref(),
            Some(ProviderError::ContextOverflow(_))
        )
    }

    fn should_retry_transient(
        &self,
        result: &anie_agent::AgentRunResult,
        retry_attempt: u32,
    ) -> bool {
        matches!(result.terminal_error.as_ref(), Some(error) if error.is_retryable())
            && retry_attempt < self.retry_config.max_retries
    }

    async fn schedule_transient_retry(
        &mut self,
        event_tx: &mpsc::Sender<AgentEvent>,
        error: &ProviderError,
        retry_attempt: u32,
    ) -> Result<()> {
        let delay_ms = retry_delay_ms(&self.retry_config, error, retry_attempt);
        let _ = event_tx
            .send(AgentEvent::RetryScheduled {
                attempt: retry_attempt,
                max_retries: self.retry_config.max_retries,
                delay_ms,
                error: error.to_string(),
            })
            .await;
        let transcript = self
            .session_context()
            .messages
            .into_iter()
            .map(|message| message.message)
            .collect::<Vec<_>>();
        let _ = event_tx
            .send(AgentEvent::TranscriptReplace {
                messages: transcript,
            })
            .await;
        let _ = event_tx.send(self.status_event()).await;
        info!(retry_attempt, delay_ms, error = %error, "scheduling transient provider retry");
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        Ok(())
    }

    async fn retry_after_overflow(&mut self, event_tx: &mpsc::Sender<AgentEvent>) -> Result<bool> {
        let _ = event_tx
            .send(AgentEvent::SystemMessage {
                text: "Context window exceeded; compacting and retrying...".into(),
            })
            .await;
        // Overflow recovery halves the keep-recent budget — we're already
        // over the context window, so we need to discard more aggressively.
        let keep_recent = (self.config.compaction.keep_recent_tokens / 2).max(1_000);
        let (config, strategy) = self.compaction_strategy(keep_recent);
        let _ = event_tx.send(AgentEvent::CompactionStart).await;
        match self
            .session
            .inner_mut()
            .force_compact(&config, &strategy)
            .await?
        {
            Some(result) => {
                self.emit_compaction_end(event_tx, &result).await;
                let transcript = self
                    .session_context()
                    .messages
                    .into_iter()
                    .map(|message| message.message)
                    .collect::<Vec<_>>();
                let _ = event_tx
                    .send(AgentEvent::TranscriptReplace {
                        messages: transcript,
                    })
                    .await;
                let _ = event_tx.send(self.status_event()).await;
                Ok(true)
            }
            None => {
                let _ = event_tx
                    .send(AgentEvent::SystemMessage {
                        text: "Context overflow recovery could not compact the session further."
                            .into(),
                    })
                    .await;
                Ok(false)
            }
        }
    }

    fn session_diff(&self) -> String {
        self.session.diff()
    }

    fn build_agent(&self) -> AgentLoop {
        AgentLoop::new(
            Arc::clone(&self.provider_registry),
            Arc::clone(&self.tool_registry),
            AgentLoopConfig {
                model: self.current_model.clone(),
                system_prompt: self.prompt_cache.current().to_string(),
                thinking: self.current_thinking,
                tool_execution: ToolExecutionMode::Parallel,
                request_options_resolver: Arc::clone(&self.request_options_resolver),
                get_steering_messages: None,
                get_follow_up_messages: None,
                before_tool_call_hook: None,
                after_tool_call_hook: None,
            },
        )
    }

    fn session_context(&self) -> SessionContext {
        self.session.context()
    }

    fn context_without_entry(&self, entry_id: Option<&str>) -> Vec<Message> {
        self.session.context_without_entry(entry_id)
    }

    fn estimated_context_tokens(&self) -> u64 {
        self.session.estimated_context_tokens()
    }

    fn status_event(&self) -> AgentEvent {
        AgentEvent::StatusUpdate {
            provider: self.current_model.provider.clone(),
            model_name: self.current_model.id.clone(),
            thinking: format_thinking(self.current_thinking),
            estimated_context_tokens: self.estimated_context_tokens(),
            context_window: self.current_model.context_window,
            cwd: self.session.cwd().display().to_string(),
            session_id: self.session.id().to_string(),
        }
    }

    fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        self.session.list()
    }

    fn apply_session_overrides(&mut self) {
        let context = self.session.context();
        if let Some((provider, model_id)) = context.model
            && let Some(model) = self
                .model_catalog
                .iter()
                .find(|candidate| candidate.provider == provider && candidate.id == model_id)
                .cloned()
                .or_else(|| {
                    fallback_model_from_provider(
                        &provider,
                        &model_id,
                        &self.config,
                        &self.model_catalog,
                    )
                })
        {
            upsert_model(&mut self.model_catalog, &model);
            self.current_model = model;
        }
        if let Some(thinking) = context.thinking_level {
            self.current_thinking = thinking;
        }
    }

    fn persist_runtime_state(&mut self) {
        self.runtime_state.provider = Some(self.current_model.provider.clone());
        self.runtime_state.model = Some(self.current_model.id.clone());
        self.runtime_state.thinking = Some(self.current_thinking);
        self.runtime_state.last_session_id = Some(self.session.id().to_string());
        if let Err(error) = save_runtime_state(&self.runtime_state) {
            warn!(%error, "failed to persist runtime state");
        }
    }

    async fn reload_config(
        &mut self,
        requested_provider: Option<&str>,
        requested_model: Option<&str>,
    ) -> Result<()> {
        let config = load_config(CliOverrides::default())?;
        let (model_catalog, local_models_available) = build_model_catalog(&config).await;
        let current_provider = requested_provider.unwrap_or(&self.current_model.provider);
        let current_model = requested_model.unwrap_or(&self.current_model.id);
        let selected_model = resolve_model(
            Some(current_provider),
            Some(current_model),
            &model_catalog,
            local_models_available,
        )
        .or_else(|_| {
            fallback_model_from_provider(current_provider, current_model, &config, &model_catalog)
                .ok_or_else(|| anyhow!("no model named '{current_model}' was found"))
        })
        .or_else(|_| {
            resolve_model(
                Some(&config.model.provider),
                Some(&config.model.id),
                &model_catalog,
                local_models_available,
            )
        })
        .or_else(|_| {
            fallback_model_from_provider(
                &config.model.provider,
                &config.model.id,
                &config,
                &model_catalog,
            )
            .ok_or_else(|| anyhow!("no model named '{}' was found", config.model.id))
        })?;

        self.config = config;
        self.model_catalog = model_catalog;
        self.current_model = selected_model;
        self.request_options_resolver = Arc::new(AuthResolver::new(
            self.cli_api_key.clone(),
            self.config.clone(),
        ));
        let cwd = self.session.cwd().to_path_buf();
        self.prompt_cache
            .replace(&cwd, &self.tool_registry, &self.config)?;
        self.persist_runtime_state();
        Ok(())
    }

    /// Rebuild the system prompt if the set of context files or any of their mtimes changed.
    fn refresh_system_prompt_if_needed(&mut self) {
        let cwd = self.session.cwd().to_path_buf();
        self.prompt_cache
            .refresh_if_stale(&cwd, &self.tool_registry, &self.config);
    }
}

async fn prepare_controller_state(cli: &Cli) -> Result<ControllerState> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let config = load_config(CliOverrides::default())?;
    let mut runtime_state = load_runtime_state().unwrap_or_default();

    let mut provider_registry = ProviderRegistry::new();
    register_builtin_providers(&mut provider_registry);
    let provider_registry = Arc::new(provider_registry);

    let (model_catalog, local_models_available) = build_model_catalog(&config).await;

    let sessions_dir =
        anie_config::anie_sessions_dir().context("home directory is not available")?;
    std::fs::create_dir_all(&sessions_dir)
        .with_context(|| format!("failed to create {}", sessions_dir.display()))?;

    let session = if let Some(session_id) = &cli.resume {
        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        SessionManager::open_session(&path).map_err(|err| {
            // Detect the AlreadyOpen variant anywhere in the error
            // chain and surface a more actionable message.
            if err.chain().any(|cause| {
                matches!(
                    cause.downcast_ref::<anie_session::SessionError>(),
                    Some(anie_session::SessionError::AlreadyOpen(_))
                )
            }) {
                anyhow::anyhow!(
                    "Session {session_id} is already open in another anie process.\n\
                     \n\
                     Options:\n\
                     - Close the other anie session and try again.\n\
                     - Use `/fork` from within the other process to branch.\n\
                     - Start a new session by omitting --resume."
                )
            } else {
                err.context(format!("failed to open session {session_id}"))
            }
        })?
    } else {
        SessionManager::new_session(&sessions_dir, &cwd)?
    };
    let session_context = session.build_context();

    let selection = resolve_initial_selection(
        cli,
        &config,
        &runtime_state,
        &session_context,
        &model_catalog,
        local_models_available,
    )?;

    let tool_registry = build_tool_registry(&cwd, cli.no_tools);
    let prompt_cache = SystemPromptCache::build(&cwd, &tool_registry, &config)?;
    let request_options_resolver: Arc<dyn RequestOptionsResolver> =
        Arc::new(AuthResolver::new(cli.api_key.clone(), config.clone()));

    let mut state = ControllerState {
        config,
        cli_api_key: cli.api_key.clone(),
        session: SessionHandle::from_manager(session, sessions_dir, cwd),
        current_model: selection.model,
        current_thinking: selection.thinking,
        model_catalog,
        provider_registry,
        tool_registry,
        request_options_resolver,
        prompt_cache,
        runtime_state: runtime_state.clone(),
        retry_config: RetryConfig::default(),
        command_registry: crate::commands::CommandRegistry::with_builtins(),
    };
    state.apply_session_overrides();
    state.persist_runtime_state();
    runtime_state = state.runtime_state.clone();
    let _ = runtime_state;
    Ok(state)
}

fn build_tool_registry(cwd: &Path, no_tools: bool) -> Arc<ToolRegistry> {
    let mut tools = ToolRegistry::new();
    if no_tools {
        return Arc::new(tools);
    }

    let queue = Arc::new(FileMutationQueue::new());
    tools.register(Arc::new(ReadTool::new(cwd.to_path_buf())));
    tools.register(Arc::new(WriteTool::with_queue(
        cwd.to_path_buf(),
        Arc::clone(&queue),
    )));
    tools.register(Arc::new(EditTool::with_queue(
        cwd.to_path_buf(),
        Arc::clone(&queue),
    )));
    tools.register(Arc::new(BashTool::new(cwd.to_path_buf())));
    Arc::new(tools)
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

async fn rpc_event_printer(mut event_rx: mpsc::Receiver<AgentEvent>) -> Result<()> {
    let stdout = tokio::io::stdout();
    let mut writer = BufWriter::new(stdout);
    while let Some(event) = event_rx.recv().await {
        let rpc_event = RpcEvent::from(event);
        let line = serde_json::to_string(&rpc_event)?;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    Ok(())
}

async fn write_rpc_error(message: &str) -> Result<()> {
    let stdout = tokio::io::stdout();
    let mut writer = BufWriter::new(stdout);
    let line = serde_json::to_string(&RpcEvent::Error {
        message: message.to_string(),
    })?;
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

fn spawn_shutdown_signal_forwarder(action_tx: mpsc::Sender<UiAction>) {
    #[cfg(not(unix))]
    let _ = action_tx;

    #[cfg(unix)]
    {
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};

            let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
                return;
            };
            let Ok(mut sighup) = signal(SignalKind::hangup()) else {
                return;
            };

            tokio::select! {
                _ = sigterm.recv() => {
                    let _ = action_tx.send(UiAction::Quit).await;
                }
                _ = sighup.recv() => {
                    let _ = action_tx.send(UiAction::Quit).await;
                }
            }
        });
    }
}

fn apply_status_event(status_bar: &mut anie_tui::StatusBarState, event: &AgentEvent) {
    if let AgentEvent::StatusUpdate {
        provider,
        model_name,
        thinking,
        estimated_context_tokens,
        context_window,
        cwd,
        session_id,
    } = event
    {
        status_bar.provider_name = provider.clone();
        status_bar.model_name = model_name.clone();
        status_bar.thinking = thinking.clone();
        status_bar.estimated_context_tokens = *estimated_context_tokens;
        status_bar.context_window = *context_window;
        status_bar.cwd = cwd.clone();
        status_bar.session_id = session_id.clone();
    }
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

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        if tokens % 1_000_000 == 0 {
            format!("{}M", tokens / 1_000_000)
        } else {
            format!("{:.1}M", tokens as f64 / 1_000_000.0)
        }
    } else if tokens >= 1_000 {
        if tokens % 1_000 == 0 {
            format!("{}k", tokens / 1_000)
        } else {
            format!("{:.1}k", tokens as f64 / 1_000.0)
        }
    } else {
        tokens.to_string()
    }
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

fn tool_hint(args: &serde_json::Value) -> String {
    if let Some(path) = args.get("path").and_then(serde_json::Value::as_str) {
        return path.to_string();
    }
    if let Some(command) = args.get("command").and_then(serde_json::Value::as_str) {
        return command.to_string();
    }
    String::new()
}

fn assistant_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum RpcCommand {
    #[serde(rename = "prompt")]
    Prompt { text: String },
    #[serde(rename = "abort")]
    Abort,
    #[serde(rename = "get_state")]
    GetState,
    #[serde(rename = "set_model")]
    SetModel {
        model: String,
        provider: Option<String>,
    },
    #[serde(rename = "set_thinking")]
    SetThinking { level: String },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum RpcEvent {
    #[serde(rename = "hello")]
    Hello { version: u32 },
    #[serde(rename = "agent_start")]
    AgentStart,
    #[serde(rename = "agent_end")]
    AgentEnd,
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "tool_exec_start")]
    ToolExecStart {
        tool: String,
        args: serde_json::Value,
    },
    #[serde(rename = "tool_exec_end")]
    ToolExecEnd { tool: String, is_error: bool },
    #[serde(rename = "transcript_replace")]
    TranscriptReplace { messages: Vec<Message> },
    #[serde(rename = "system")]
    System { text: String },
    #[serde(rename = "status")]
    Status {
        provider: String,
        model: String,
        thinking: String,
        estimated_context_tokens: u64,
        context_window: u64,
        cwd: String,
        session_id: String,
    },
    #[serde(rename = "compaction_start")]
    CompactionStart,
    #[serde(rename = "compaction_end")]
    CompactionEnd {
        summary: String,
        tokens_before: u64,
        tokens_after: u64,
    },
    #[serde(rename = "retry_scheduled")]
    RetryScheduled {
        attempt: u32,
        max_retries: u32,
        delay_ms: u64,
        error: String,
    },
    #[serde(rename = "error")]
    Error { message: String },
}

impl From<AgentEvent> for RpcEvent {
    fn from(value: AgentEvent) -> Self {
        match value {
            AgentEvent::AgentStart => Self::AgentStart,
            AgentEvent::AgentEnd { .. } => Self::AgentEnd,
            AgentEvent::MessageDelta {
                delta: StreamDelta::TextDelta(text),
            } => Self::TextDelta { text },
            AgentEvent::ToolExecStart {
                tool_name, args, ..
            } => Self::ToolExecStart {
                tool: tool_name,
                args,
            },
            AgentEvent::ToolExecEnd {
                result, is_error, ..
            } => Self::ToolExecEnd {
                tool: result
                    .details
                    .get("tool_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                is_error,
            },
            AgentEvent::TranscriptReplace { messages } => Self::TranscriptReplace { messages },
            AgentEvent::SystemMessage { text } => Self::System { text },
            AgentEvent::StatusUpdate {
                provider,
                model_name,
                thinking,
                estimated_context_tokens,
                context_window,
                cwd,
                session_id,
            } => Self::Status {
                provider,
                model: model_name,
                thinking,
                estimated_context_tokens,
                context_window,
                cwd,
                session_id,
            },
            AgentEvent::CompactionStart => Self::CompactionStart,
            AgentEvent::CompactionEnd {
                summary,
                tokens_before,
                tokens_after,
            } => Self::CompactionEnd {
                summary,
                tokens_before,
                tokens_after,
            },
            AgentEvent::RetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => Self::RetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                error,
            },
            AgentEvent::TurnStart
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageEnd { .. }
            | AgentEvent::MessageDelta { .. }
            | AgentEvent::ToolExecUpdate { .. } => Self::System {
                text: String::new(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, thread, time::Duration};

    use super::*;
    use crate::model_catalog::dedupe_models;
    use anie_provider::{
        ApiKind, CostPerMillion, ProviderError, ReasoningCapabilities, ReasoningControlMode,
        ReasoningOutputMode, ThinkingRequestMode,
    };
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
        }
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
    fn dedupe_models_keeps_later_entries_for_same_provider_and_id() {
        let mut models = vec![
            model("o4-mini", "openai"),
            Model {
                max_tokens: 16_384,
                supports_reasoning: true,
                reasoning_capabilities: Some(ReasoningCapabilities {
                    control: Some(ReasoningControlMode::Native),
                    output: Some(ReasoningOutputMode::Separated),
                    tags: None,
                    request_mode: Some(ThinkingRequestMode::ReasoningEffort),
                }),
                ..model("o4-mini", "openai")
            },
        ];

        dedupe_models(&mut models);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].max_tokens, 16_384);
        assert!(models[0].supports_reasoning);
        assert_eq!(
            models[0].reasoning_capabilities,
            Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Native),
                output: Some(ReasoningOutputMode::Separated),
                tags: None,
                request_mode: Some(ThinkingRequestMode::ReasoningEffort),
            })
        );
    }

    #[test]
    fn resolve_model_honors_provider_and_id() {
        let models = vec![model("gpt-4o", "openai"), model("qwen3:32b", "ollama")];
        let resolved =
            resolve_model(Some("ollama"), Some("qwen3:32b"), &models, true).expect("resolve model");
        assert_eq!(resolved.provider, "ollama");
        assert_eq!(resolved.id, "qwen3:32b");
    }

    #[test]
    fn resolve_model_prefers_local_when_no_hints() {
        let models = vec![model("gpt-4o", "openai"), model("qwen3:32b", "ollama")];
        let resolved = resolve_model(None, None, &models, true).expect("resolve model");
        assert_eq!(resolved.provider, "ollama");
    }

    #[test]
    fn resolve_initial_selection_prefers_provider_only_override() {
        let cli = Cli {
            command: None,
            interactive: false,
            print: true,
            rpc: false,
            no_tools: false,
            prompt: vec!["hello".into()],
            model: None,
            provider: Some("ollama".into()),
            api_key: None,
            thinking: None,
            resume: None,
            cwd: None,
        };
        let config = AnieConfig::default();
        let runtime_state = RuntimeState::default();
        let session_context = SessionContext::empty();
        let models = vec![model("gpt-4o", "openai"), model("qwen3:32b", "ollama")];

        let selection = resolve_initial_selection(
            &cli,
            &config,
            &runtime_state,
            &session_context,
            &models,
            true,
        )
        .expect("resolve selection");
        assert_eq!(selection.model.provider, "ollama");
    }

    #[test]
    fn retry_delay_prefers_retry_after_header() {
        let config = RetryConfig {
            initial_delay_ms: 1_000,
            max_delay_ms: 30_000,
            backoff_multiplier: 2.0,
            max_retries: 3,
            jitter: false,
        };
        let error = ProviderError::RateLimited {
            retry_after_ms: Some(7_000),
        };
        assert_eq!(retry_delay_ms(&config, &error, 1), 7_000);
    }

    #[test]
    fn retry_delay_uses_exponential_backoff() {
        let config = RetryConfig {
            initial_delay_ms: 1_000,
            max_delay_ms: 30_000,
            backoff_multiplier: 2.0,
            max_retries: 3,
            jitter: false,
        };
        let error = ProviderError::MalformedStreamEvent("socket dropped".into());
        assert_eq!(retry_delay_ms(&config, &error, 1), 1_000);
        assert_eq!(retry_delay_ms(&config, &error, 2), 2_000);
        assert_eq!(retry_delay_ms(&config, &error, 3), 4_000);
    }

    #[test]
    fn context_files_stamp_detects_deleted_non_newest_file() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        let nested = root.join("src/module");
        fs::create_dir_all(&nested).expect("create nested dirs");

        let project_agents = root.join("AGENTS.md");
        let nested_claude = nested.join("CLAUDE.md");
        fs::write(&project_agents, "project context").expect("write agents");
        thread::sleep(Duration::from_millis(5));
        fs::write(&nested_claude, "nested context").expect("write claude");

        let config = AnieConfig::default();
        let first = context_files_stamp(&nested, &config);
        assert_eq!(first.len(), 2);

        fs::remove_file(&project_agents).expect("remove agents");
        let second = context_files_stamp(&nested, &config);

        assert_ne!(
            first, second,
            "deleting a non-newest file should change stamp"
        );
        assert_eq!(second.len(), 1);
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
}
