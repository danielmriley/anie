use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use time::{OffsetDateTime, format_description::FormatItem, macros::format_description};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use anie_agent::{AgentLoop, AgentLoopConfig, ToolExecutionMode, ToolRegistry};
use anie_auth::AuthResolver;
use anie_config::{AnieConfig, collect_context_files};
use anie_protocol::{
    AgentEvent, AssistantMessage, ContentBlock, Message, StopReason, Usage, UserMessage, now_millis,
};
use anie_provider::{
    ApiKind, Model, ProviderError, ProviderRegistry, RequestOptionsResolver, ThinkingLevel,
};
use anie_session::{CompactionConfig, SessionContext, SessionInfo};
use anie_tui::UiAction;

use crate::compaction_stats::{CompactionStats, CompactionStatsAtomic};
use crate::retry_policy::GiveUpReason;
use crate::{
    Cli,
    compaction::CompactionStrategy,
    model_catalog::{resolve_requested_model, upsert_model},
    runtime::{ConfigState, SessionHandle, SystemPromptCache},
    user_error::{HandleError, UserCommandError, render_user_facing_provider_error},
};

const DATE_FORMAT: &[FormatItem<'static>] = format_description!("[year]-[month]-[day]");
const MIN_OLLAMA_CONTEXT_LENGTH: u64 = 2_048;
const MAX_OLLAMA_CONTEXT_LENGTH: u64 = 1_048_576;

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
    ui_action_rx: mpsc::UnboundedReceiver<UiAction>,
    event_tx: mpsc::Sender<AgentEvent>,
    current_run: Option<CurrentRun>,
    /// Between-runs state for a pending transient retry. Set when
    /// the retry policy decides to back off; cleared either when
    /// the deadline fires (we start the continuation) or when the
    /// user aborts/quits during the wait. Holding this as state
    /// instead of an inline `tokio::time::sleep(...)` lets the
    /// main `select!` keep polling `ui_action_rx` throughout the
    /// backoff, which is what makes Ctrl+C responsive during
    /// retries.
    pending_retry: PendingRetry,
    quitting: bool,
    exit_after_run: bool,
    /// FIFO queue of follow-up prompts the user submitted while
    /// a run was active. Drained one-at-a-time at the run-
    /// completion boundary in the main loop. Plan 02 of
    /// `docs/active_input_2026-04-27/`. Persisted in memory only
    /// — if anie crashes mid-queue the unstarted prompts are
    /// lost; that's the documented trade-off in the plan.
    queued_prompts: VecDeque<String>,
    /// Compactions still allowed in the current user turn.
    /// Reset to `[compaction] max_per_turn` at the top of every
    /// `start_prompt_run`; decremented in `emit_compaction_end`
    /// after every successful compaction (pre-prompt and
    /// reactive paths). Read-only consumers
    /// (`compaction_budget_remaining`) use `Acquire` loads;
    /// writers use `Release` stores. The `Arc` shape is
    /// chosen so plan 04's mid-turn gate can clone the handle
    /// into a spawned task without moving it. Plan
    /// `docs/midturn_compaction_2026-04-27/02_per_turn_compaction_budget.md`.
    compactions_remaining_this_turn: Arc<AtomicU32>,
}

struct CurrentRun {
    handle: JoinHandle<anie_agent::AgentRunResult>,
    cancel: CancellationToken,
    already_compacted: bool,
    retry_attempt: u32,
}

/// The between-runs timer for transient-retry backoff.
///
/// `Idle` is the default state after a run completes. `Armed`
/// records the future continuation: its deadline, and the retry
/// bookkeeping (`attempt`, `already_compacted`) that a fresh
/// `CurrentRun` would otherwise carry. On deadline fire the
/// controller starts a continuation run with the captured values;
/// on user abort/quit the controller clears the state without
/// starting anything — but PR A of
/// `docs/run_abort_breadcrumb_2026-04-28/` extends `Armed` with
/// the failed run's `error`/`provider`/`model` so PR B can write
/// an error-assistant breadcrumb to the session before clearing.
///
/// No longer `Copy` because `ProviderError` and the `String`
/// fields aren't `Copy`. `Clone` is enough — we never need to
/// duplicate the state, just match-by-reference.
#[derive(Debug, Clone)]
enum PendingRetry {
    Idle,
    Armed {
        deadline: Instant,
        attempt: u32,
        already_compacted: bool,
        /// The `ProviderError` that triggered the scheduled
        /// retry. Cloned so the breadcrumb path can render
        /// the same error string the user already saw via
        /// `RetryScheduled`.
        error: ProviderError,
        /// `provider` and `model` are captured at retry-arm
        /// time rather than re-read at cancel time so a model
        /// switch between arming and canceling produces a
        /// breadcrumb attributed to the original failed run,
        /// not to whatever the controller has currently
        /// selected.
        provider: String,
        model: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ContextLengthMutation {
    /// Override applied. `above_cap_warning` carries an
    /// optional message when the value exceeded the
    /// workspace-wide `[ollama] default_max_num_ctx` cap; the
    /// caller emits it as a separate system message so the
    /// user sees both the success and the conflict.
    Set {
        above_cap_warning: Option<String>,
    },
    Reset,
}

impl InteractiveController {
    pub(crate) fn new(
        state: ControllerState,
        ui_action_rx: mpsc::UnboundedReceiver<UiAction>,
        event_tx: mpsc::Sender<AgentEvent>,
        exit_after_run: bool,
    ) -> Self {
        let max_per_turn = state.config.anie_config().compaction.max_per_turn;
        Self {
            state,
            ui_action_rx,
            event_tx,
            current_run: None,
            pending_retry: PendingRetry::Idle,
            quitting: false,
            exit_after_run,
            queued_prompts: VecDeque::new(),
            compactions_remaining_this_turn: Arc::new(AtomicU32::new(max_per_turn)),
        }
    }

    /// Read-only accessor for the per-turn compaction budget.
    /// Used by the reactive retry path (`RetryPolicy::decide`)
    /// and — once plan 04 lands — the mid-turn gate's pre-check.
    /// PR 8.2 of `docs/midturn_compaction_2026-04-27/`.
    fn compaction_budget_remaining(&self) -> u32 {
        self.compactions_remaining_this_turn.load(Ordering::Acquire)
    }

    /// Build a `ControllerCompactionGate` snapshot for the
    /// current turn, or `None` when compaction is disabled.
    /// Each gate carries a per-turn snapshot of the
    /// effective `CompactionConfig` and a freshly-built
    /// summarizer; `Arc`-cloned with the controller's
    /// per-turn budget atomic so the mid-turn path
    /// decrements the same counter the pre-prompt and
    /// reactive paths consume.
    /// PR 8.4 PR B of `docs/midturn_compaction_2026-04-27/`.
    fn build_compaction_gate(&self) -> Option<Arc<dyn anie_agent::CompactionGate>> {
        if !self.state.config.anie_config().compaction.enabled {
            return None;
        }
        // Plan `docs/rlm_2026-04-29/07_evaluation_harness.md`:
        // baseline mode opts out of the compaction gate so
        // measurements isolate the model's raw behavior. The
        // user's eventual `Skipped` reasons / overflow errors
        // are then attributable to the model, not the
        // harness.
        if !self.state.harness_mode.installs_compaction_gate() {
            return None;
        }
        let (config, strategy) = self.state.compaction_strategy(
            self.state
                .config
                .anie_config()
                .compaction
                .keep_recent_tokens,
        );
        let gate = crate::compaction_gate::ControllerCompactionGate {
            config,
            summarizer: Arc::new(strategy),
            budget: Arc::clone(&self.compactions_remaining_this_turn),
            event_tx: self.event_tx.clone(),
            stats: Arc::clone(&self.state.compaction_stats),
            // Stagnation history is per-turn: a fresh
            // `Default::default()` gives an empty history and
            // `aggressive_level: 0`, matching the per-turn
            // semantics of the existing budget reset.
            state: Arc::new(std::sync::Mutex::new(
                crate::compaction_gate::GateState::default(),
            )),
        };
        Some(Arc::new(gate) as Arc<dyn anie_agent::CompactionGate>)
    }

    /// Drain the next queued follow-up prompt, if any, and start
    /// it as a new run. Called at the run-completion boundary in
    /// the main loop after `finish_run` has persisted the just-
    /// completed run's messages — this preserves session order
    /// (current run's assistant content lands before the queued
    /// user prompt). Returns `true` if a prompt was started.
    async fn try_drain_queued_prompt(&mut self) -> Result<bool> {
        let Some(text) = self.queued_prompts.pop_front() else {
            return Ok(false);
        };
        let preview: String = text.lines().next().unwrap_or("").chars().take(80).collect();
        let _ = self
            .event_tx
            .send(AgentEvent::SystemMessage {
                text: format!("Starting queued follow-up: {preview}"),
            })
            .await;
        self.start_prompt_run(text).await?;
        Ok(true)
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
            // Three-way state dispatch. Each arm polls
            // `ui_action_rx` so user actions are never ignored
            // while a run is in flight or a retry is armed.
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
                                    let budget_remaining = self.compaction_budget_remaining();
                                    match policy.decide(
                                        error,
                                        retry_attempt,
                                        already_compacted,
                                        budget_remaining,
                                    ) {
                                        RetryDecision::Compact => {
                                            match self.state.retry_after_overflow(&self.event_tx).await {
                                                Ok(true) => {
                                                    // Successful reactive compaction
                                                    // consumes a budget slot. PR 8.2 of
                                                    // `docs/midturn_compaction_2026-04-27/`.
                                                    self.compactions_remaining_this_turn
                                                        .fetch_update(
                                                            Ordering::Release,
                                                            Ordering::Acquire,
                                                            |n| Some(n.saturating_sub(1)),
                                                        )
                                                        .ok();
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
                                            // PR 2.3 of `active_input_2026-04-27/`. If
                                            // the user has queued follow-up prompts,
                                            // their input is the freshest signal and
                                            // should not wait behind a stale automatic
                                            // retry. Finish the failed run, surface a
                                            // short note, and let the run-completion
                                            // drain start the queued prompt.
                                            if !self.queued_prompts.is_empty() {
                                                anie_agent::send_event(
                                                    &self.event_tx,
                                                    AgentEvent::SystemMessage {
                                                        text: "Skipping automatic retry because a follow-up is queued.".into(),
                                                    },
                                                )
                                                .await;
                                                self.state.finish_run(&result).await?;
                                            } else {
                                                // Emit RetryScheduled and arm the backoff state.
                                                // The main loop's PendingRetry arm will fire
                                                // the continuation when the deadline elapses.
                                                self.state
                                                    .emit_retry_scheduled(
                                                        &self.event_tx,
                                                        error,
                                                        attempt,
                                                        delay_ms,
                                                    )
                                                    .await?;
                                                let model = self.state.config.current_model();
                                                self.pending_retry = PendingRetry::Armed {
                                                    deadline: Instant::now() + Duration::from_millis(delay_ms),
                                                    attempt,
                                                    already_compacted,
                                                    error: error.clone(),
                                                    provider: model.provider.clone(),
                                                    model: model.id.clone(),
                                                };
                                            }
                                        }
                                        RetryDecision::GiveUp { reason } => {
                                            info!(?reason, retry_attempt, error = %error, "not retrying provider error");
                                            // Surface a user-facing message for variants
                                            // that carry actionable recovery context
                                            // (currently: ModelLoadResources →
                                            // /context-length suggestion). Other
                                            // variants stay log-only — their default
                                            // Display is already adequate and the
                                            // existing no-message-on-give-up behavior
                                            // is preserved to avoid scope creep.
                                            // See docs/ollama_load_failure_recovery
                                            // README PR 3.
                                            // Use the *effective* num_ctx — the value
                                            // actually sent to Ollama on the wire — so the
                                            // user-facing message reports the failed attempt
                                            // accurately when a runtime `/context-length`
                                            // override is active. Without this, a user who
                                            // ran `/context-length 65536` on a model with
                                            // discovered context_window 262144 would see the
                                            // message claim Ollama tried 262144 / 131072,
                                            // when it actually tried 65536 / 32768.
                                            // PR 8.2 of `docs/midturn_compaction_2026-04-27/`.
                                            // `CompactionBudgetExhausted` carries no
                                            // model-load-resources detail, so render an
                                            // actionable message here rather than relying on
                                            // `render_user_facing_provider_error` (which keys
                                            // off the underlying ProviderError, not the
                                            // give-up reason).
                                            if matches!(
                                                reason,
                                                GiveUpReason::CompactionBudgetExhausted
                                            ) {
                                                let max_per_turn = self
                                                    .state
                                                    .config
                                                    .anie_config()
                                                    .compaction
                                                    .max_per_turn;
                                                anie_agent::send_event(
                                                    &self.event_tx,
                                                    AgentEvent::SystemMessage {
                                                        text: format!(
                                                            "Context overflow; the per-turn compaction budget ({max_per_turn}) is already used. \
                                                             Try a smaller prompt, raise /context-length, or set [compaction] max_per_turn higher."
                                                        ),
                                                    },
                                                )
                                                .await;
                                            } else {
                                                let model = self.state.config.current_model();
                                                let requested_num_ctx =
                                                    self.state.config.effective_ollama_context_window();
                                                if let Some(message) =
                                                    render_user_facing_provider_error(
                                                        error,
                                                        requested_num_ctx,
                                                        &model.provider,
                                                        &model.id,
                                                    )
                                                {
                                                    anie_agent::send_event(
                                                        &self.event_tx,
                                                        AgentEvent::SystemMessage {
                                                            text: message,
                                                        },
                                                    )
                                                    .await;
                                                }
                                            }
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
                        // Drain the next queued follow-up
                        // (plan 02 of active_input). Done
                        // *after* the just-completed run's
                        // messages were persisted by
                        // `finish_run` so session order is
                        // current-assistant → queued-user.
                        // Skipped when a transient retry is
                        // armed; PR 2.3 will give queued
                        // prompts priority over stale retries.
                        if self.current_run.is_none()
                            && matches!(self.pending_retry, PendingRetry::Idle)
                        {
                            self.try_drain_queued_prompt().await?;
                        }

                        if self.exit_after_run
                            && self.current_run.is_none()
                            && matches!(self.pending_retry, PendingRetry::Idle)
                            && self.queued_prompts.is_empty()
                        {
                            self.quitting = true;
                        }
                    }
                }
            } else if let PendingRetry::Armed {
                deadline,
                attempt,
                already_compacted,
                ..
            } = &self.pending_retry
            {
                // PendingRetry is no longer `Copy` (PR A of
                // `docs/run_abort_breadcrumb_2026-04-28/`), so
                // copy out the primitives via `*` and let the
                // borrow end before the `select!` body — which
                // re-borrows `self` mutably to handle UI actions
                // or to clear `pending_retry` on deadline fire.
                let deadline = *deadline;
                let attempt = *attempt;
                let already_compacted = *already_compacted;
                tokio::select! {
                    maybe_action = self.ui_action_rx.recv() => {
                        match maybe_action {
                            Some(action) => self.handle_action(action).await?,
                            None => {
                                // Upstream hung up while backoff
                                // was armed. Write the breadcrumb
                                // and fall through to the quit path
                                // so the session log isn't left with
                                // a dangling user message. Plan
                                // `docs/run_abort_breadcrumb_2026-04-28/`.
                                self.abort_pending_retry().await?;
                                self.quitting = true;
                            }
                        }
                    }
                    _ = sleep_until(deadline) => {
                        self.pending_retry = PendingRetry::Idle;
                        self.start_continuation_run(already_compacted, attempt).await?;
                    }
                }
            } else {
                match self.ui_action_rx.recv().await {
                    Some(action) => self.handle_action(action).await?,
                    None => break,
                }
            }

            if self.quitting
                && self.current_run.is_none()
                && matches!(self.pending_retry, PendingRetry::Idle)
            {
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
            UiAction::QueuePrompt(text) => {
                // Plan 02 of `docs/active_input_2026-04-27/`.
                // While a run is active, push onto the FIFO
                // queue; the main loop drains it after each
                // run completes. While idle, start the prompt
                // immediately (matches the SubmitPrompt shape
                // for callers that emit QueuePrompt
                // unconditionally). PR 2.3 added the
                // pending-retry override: a queued prompt is a
                // fresh user signal, so a stale armed retry
                // should yield to it.
                if self.current_run.is_some() {
                    let preview: String =
                        text.lines().next().unwrap_or("").chars().take(80).collect();
                    self.queued_prompts.push_back(text);
                    let position = self.queued_prompts.len();
                    let _ = self
                        .event_tx
                        .send(AgentEvent::SystemMessage {
                            text: format!(
                                "Queued follow-up #{position}: {preview} (will run after the current response)",
                            ),
                        })
                        .await;
                } else if matches!(self.pending_retry, PendingRetry::Armed { .. }) {
                    // A retry was armed but the user typed a
                    // new prompt — write the breadcrumb (Plan
                    // `docs/run_abort_breadcrumb_2026-04-28/`),
                    // then start the prompt now.
                    self.abort_pending_retry().await?;
                    let _ = self
                        .event_tx
                        .send(AgentEvent::SystemMessage {
                            text: "Cancelling pending retry to start your follow-up.".into(),
                        })
                        .await;
                    self.start_prompt_run(text).await?;
                } else {
                    self.start_prompt_run(text).await?;
                }
            }
            UiAction::AbortAndQueuePrompt(text) => {
                // Plan 03 of `docs/active_input_2026-04-27/`.
                // The user has a draft they want to send *now*.
                // Three cases:
                //   - active run: front-queue the draft and
                //     cancel the run. The post-run drain will
                //     pick the front-queued prompt up before any
                //     stale FIFO-queued follow-ups.
                //   - pending retry armed: clear the retry and
                //     start the prompt immediately (matches
                //     `QueuePrompt` semantics; the user's fresh
                //     signal beats a transient-error retry).
                //   - idle: start the prompt immediately.
                if let Some(current_run) = &self.current_run {
                    let preview: String =
                        text.lines().next().unwrap_or("").chars().take(80).collect();
                    self.queued_prompts.push_front(text);
                    current_run.cancel.cancel();
                    let _ = self
                        .event_tx
                        .send(AgentEvent::SystemMessage {
                            text: format!(
                                "Aborting current run; queued draft will send next: {preview}",
                            ),
                        })
                        .await;
                } else if matches!(self.pending_retry, PendingRetry::Armed { .. }) {
                    self.abort_pending_retry().await?;
                    let _ = self
                        .event_tx
                        .send(AgentEvent::SystemMessage {
                            text: "Cancelling pending retry to start your interrupt.".into(),
                        })
                        .await;
                    self.start_prompt_run(text).await?;
                } else {
                    self.start_prompt_run(text).await?;
                }
            }
            UiAction::Abort => {
                if let Some(current_run) = &self.current_run {
                    current_run.cancel.cancel();
                } else if matches!(self.pending_retry, PendingRetry::Armed { .. }) {
                    self.abort_pending_retry().await?;
                    self.send_system_message("Retry aborted by user.").await;
                }
            }
            UiAction::Quit => {
                self.quitting = true;
                if let Some(current_run) = &self.current_run {
                    current_run.cancel.cancel();
                }
                // A pending retry is in-memory state; finalize it
                // (writing the session breadcrumb if one is armed)
                // and tear it down so the outer quit-check ends the
                // loop in the next iteration instead of waiting for
                // the deadline. Plan
                // `docs/run_abort_breadcrumb_2026-04-28/`.
                self.abort_pending_retry().await?;
            }
            UiAction::SetModel(requested) => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot change models while a run is active.")
                        .await;
                } else {
                    let persistence_warning = self.state.set_model(&requested).await?;
                    self.cancel_and_emit_status().await?;
                    self.send_system_message(&format!(
                        "Model set to {}:{}",
                        self.state.config.current_model().provider,
                        self.state.config.current_model().id,
                    ))
                    .await;
                    self.send_persistence_warning_if_present(persistence_warning)
                        .await;
                }
            }
            UiAction::SetResolvedModel(model) => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot change models while a run is active.")
                        .await;
                } else {
                    let persistence_warning = self.state.set_model_resolved(*model).await?;
                    self.cancel_and_emit_status().await?;
                    self.send_persistence_warning_if_present(persistence_warning)
                        .await;
                }
            }
            UiAction::SetThinking(level) => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot change thinking while a run is active.")
                        .await;
                } else {
                    let persistence_warning = self.state.set_thinking(&level).await?;
                    self.cancel_and_emit_status().await?;
                    self.send_system_message(&format!(
                        "Thinking level set to {}",
                        format_thinking(self.state.config.current_thinking()),
                    ))
                    .await;
                    self.send_persistence_warning_if_present(persistence_warning)
                        .await;
                }
            }
            UiAction::ContextLength(argument) => {
                if !self.state.current_model_uses_ollama_chat_api() {
                    self.send_system_message(&self.state.context_length_non_ollama_message())
                        .await;
                } else if argument.is_none() {
                    self.send_system_message(&self.state.context_length_status_message())
                        .await;
                } else if self.current_run.is_some() {
                    self.send_system_message("Cannot change context length while a run is active.")
                        .await;
                } else if matches!(self.pending_retry, PendingRetry::Armed { .. }) {
                    self.send_system_message(
                        "Cannot change context length while a retry is pending. Abort the retry first.",
                    )
                    .await;
                } else if let Some(argument) = argument {
                    match self.state.apply_context_length_argument(&argument) {
                        Ok(ContextLengthMutation::Set { above_cap_warning }) => {
                            anie_agent::send_event(&self.event_tx, self.state.status_event()).await;
                            self.send_system_message(&format!(
                                "Context window set to {}. Ollama will reload the model on the next request (~5-30 s for this model).",
                                format_context_length(
                                    self.state.config.effective_ollama_context_window()
                                ),
                            ))
                            .await;
                            // Emit the above-cap warning as a
                            // separate system message so it
                            // doesn't get lost in the success
                            // text. Cap PR 3.
                            if let Some(warning) = above_cap_warning {
                                self.send_system_message(&warning).await;
                            }
                            let warning = self
                                .state
                                .persist_runtime_state_warning("context_length_set");
                            self.send_persistence_warning_if_present(warning).await;
                        }
                        Ok(ContextLengthMutation::Reset) => {
                            anie_agent::send_event(&self.event_tx, self.state.status_event()).await;
                            self.send_system_message(&format!(
                                "Context window reset to {}.",
                                format_context_length(
                                    self.state.config.effective_ollama_context_window()
                                ),
                            ))
                            .await;
                            let warning = self
                                .state
                                .persist_runtime_state_warning("context_length_reset");
                            self.send_persistence_warning_if_present(warning).await;
                        }
                        Err(message) => self.send_system_message(&message).await,
                    }
                }
            }
            UiAction::ShowState => {
                let summary = self.state.state_summary_message();
                self.send_system_message(&summary).await;
            }
            UiAction::Compact => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot compact while a run is active.")
                        .await;
                } else {
                    self.state.force_compact(&self.event_tx).await?;
                    self.cancel_pending_retry_for_run_affecting_change().await?;
                }
            }
            UiAction::ForkSession => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot fork while a run is active.")
                        .await;
                } else {
                    let new_session_id = self.state.fork_session().await?;
                    self.cancel_pending_retry_for_run_affecting_change().await?;
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
                self.send_system_message(&self.state.session.diff()).await;
            }
            UiAction::NewSession => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot start a new session while a run is active.")
                        .await;
                } else {
                    self.state.new_session().await?;
                    self.cancel_pending_retry_for_run_affecting_change().await?;
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
                let sessions = self.state.session.list()?;
                self.send_system_message(&format_sessions(&sessions, self.state.session.id()))
                    .await;
            }
            UiAction::SwitchSession(session_id) => {
                if self.current_run.is_some() {
                    self.send_system_message("Cannot switch sessions while a run is active.")
                        .await;
                } else {
                    self.state.switch_session(&session_id).await?;
                    self.cancel_pending_retry_for_run_affecting_change().await?;
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
                    self.cancel_pending_retry_for_run_affecting_change().await?;
                    anie_agent::send_event(&self.event_tx, self.state.status_event()).await;
                    self.send_system_message("Configuration reloaded.").await;
                }
            }
            UiAction::ClearOutput => {}
        }
        Ok(())
    }

    async fn cancel_pending_retry_for_run_affecting_change(&mut self) -> Result<()> {
        if matches!(self.pending_retry, PendingRetry::Armed { .. }) {
            self.abort_pending_retry().await?;
            self.send_system_message("Pending retry canceled because run settings changed.")
                .await;
        }
        Ok(())
    }

    /// Finalize a pending retry as a failed turn before clearing
    /// it.
    ///
    /// When the controller cancels `PendingRetry::Armed` for any
    /// reason other than the deadline firing — a fresh user
    /// prompt, abort, quit, session change, or model switch —
    /// this writes a synthetic error-assistant message into the
    /// session log so the transcript preserves the invariant
    /// that every user message has a following assistant
    /// message. Without this breadcrumb, a later model taking
    /// over the same session sees back-to-back user messages
    /// (the failed turn's prompt + the new prompt) and
    /// reconstructs history incorrectly.
    ///
    /// No-op when state is already `Idle`. The error string and
    /// `provider`/`model` are taken from the captured retry
    /// context (PR A) so the breadcrumb is attributed to the
    /// run that actually failed, not to whatever model the
    /// controller has currently selected.
    ///
    /// Plan `docs/run_abort_breadcrumb_2026-04-28/`.
    async fn abort_pending_retry(&mut self) -> Result<()> {
        let PendingRetry::Armed {
            error,
            provider,
            model,
            ..
        } = std::mem::replace(&mut self.pending_retry, PendingRetry::Idle)
        else {
            return Ok(());
        };
        let error_string = error.to_string();
        let assistant = AssistantMessage {
            content: vec![ContentBlock::Text {
                text: error_string.clone(),
            }],
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some(error_string),
            provider,
            model,
            timestamp: now_millis(),
            reasoning_details: None,
        };
        self.state
            .session
            .inner_mut()
            .append_message(&Message::Assistant(assistant))?;
        Ok(())
    }

    async fn start_prompt_run(&mut self, text: String) -> Result<()> {
        info!(
            provider = %self.state.config.current_model().provider,
            model = %self.state.config.current_model().id,
            thinking = %format_thinking(self.state.config.current_thinking()),
            "starting interactive run"
        );
        // A fresh user prompt supersedes any pending retry from
        // the previous turn — without this, the retry's continuation
        // would spawn after the new prompt finishes and interleave
        // on stale context. Write the breadcrumb before clearing so
        // the session log records what happened to the failed turn.
        // Plan `docs/run_abort_breadcrumb_2026-04-28/`.
        if matches!(self.pending_retry, PendingRetry::Armed { .. }) {
            info!("cancelling pending retry in favor of new prompt");
            self.abort_pending_retry().await?;
        }
        // Reset the per-turn compaction budget. Each fresh user
        // prompt earns the configured allowance — the budget only
        // protects against compaction storms *within* a single
        // turn. PR 8.2 of `docs/midturn_compaction_2026-04-27/`.
        let max_per_turn = self.state.config.anie_config().compaction.max_per_turn;
        self.compactions_remaining_this_turn
            .store(max_per_turn, Ordering::Release);
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
        if self.state.config.anie_config().compaction.enabled
            && self.state.maybe_auto_compact(&self.event_tx).await?
        {
            // Successful pre-prompt compaction consumes one of
            // this turn's budget slots. PR 8.2 of
            // `docs/midturn_compaction_2026-04-27/`.
            self.compactions_remaining_this_turn
                .fetch_update(Ordering::Release, Ordering::Acquire, |n| {
                    Some(n.saturating_sub(1))
                })
                .ok();
        }
        let context = self
            .state
            .session
            .context_without_entry(Some(&prompt_entry_id));
        let agent = build_agent(&self.state, self.build_compaction_gate());
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let event_tx = self.event_tx.clone();
        let handle = tokio::spawn(async move {
            run_via_step_machine(
                &agent,
                vec![prompt_message],
                context,
                &event_tx,
                &task_cancel,
            )
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
        let agent = build_agent(&self.state, self.build_compaction_gate());
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let event_tx = self.event_tx.clone();
        let handle = tokio::spawn(async move {
            run_via_step_machine(&agent, Vec::new(), context, &event_tx, &task_cancel).await
        });
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

    /// Send a system-message follow-up containing a persistence
    /// warning if the persistence call returned one. Several
    /// config-mutation arms share this exact shape; the helper
    /// removes 3 lines per call site.
    /// PR 01 of `docs/code_consolidation_2026-04-26/`.
    async fn send_persistence_warning_if_present(&self, warning: Option<String>) {
        if let Some(text) = warning {
            self.send_system_message(&text).await;
        }
    }

    /// Cancel any pending retry whose continuation depends on
    /// model / thinking / context state, then push a fresh
    /// `StatusUpdate` to the UI. Several config-mutation arms
    /// share this two-step pattern; the helper consolidates.
    /// PR 01 of `docs/code_consolidation_2026-04-26/`.
    async fn cancel_and_emit_status(&mut self) -> Result<()> {
        self.cancel_pending_retry_for_run_affecting_change().await?;
        anie_agent::send_event(&self.event_tx, self.state.status_event()).await;
        Ok(())
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
    /// Per-session running counts of compaction events by
    /// phase. Surfaced in `/state`, reset on `/new` and
    /// session switch. Shared via `Arc` with the mid-turn
    /// `ControllerCompactionGate` so all paths increment the
    /// same atomic. Plan 06 of
    /// `docs/midturn_compaction_2026-04-27/`.
    pub(crate) compaction_stats: Arc<CompactionStatsAtomic>,
    /// Harness profile selected at startup via `--harness-mode`.
    /// Determines which capabilities the harness exposes:
    /// `Baseline` (no tools, no gate, no policies),
    /// `Current` (today's behavior), or `Rlm` (Plan 06
    /// context virtualization — currently identical to
    /// `Current` until later commits land the recurse tool
    /// and the active-context policy). Plan
    /// `docs/rlm_2026-04-29/07_evaluation_harness.md`.
    pub(crate) harness_mode: crate::harness_mode::HarnessMode,
}

impl ControllerState {
    pub(crate) fn persist_runtime_state(&mut self) -> Result<()> {
        self.config.persist_runtime_state(self.session.id())
    }

    fn persist_runtime_state_logged(&mut self, context: &'static str) {
        if let Err(error) = self.persist_runtime_state() {
            warn!(%error, context, "failed to persist runtime state");
        }
    }

    fn persist_runtime_state_warning(&mut self, context: &'static str) -> Option<String> {
        self.persist_runtime_state().err().map(|error| {
            warn!(%error, context, "failed to persist runtime state");
            format!(
                "Warning: setting is active for this session, but anie could not save it for the next launch; it may revert after restart: {error}"
            )
        })
    }

    async fn set_model(&mut self, requested: &str) -> Result<Option<String>> {
        let model = resolve_requested_model(
            requested,
            &self.config.current_model().provider,
            &self.model_catalog,
        )
        .map_err(|_| UserCommandError::UnknownModel(requested.to_string()))?;
        self.set_model_resolved(model).await
    }

    async fn set_model_resolved(&mut self, model: Model) -> Result<Option<String>> {
        upsert_model(&mut self.model_catalog, &model);
        self.config.set_model(model);
        self.session.inner_mut().append_model_change(
            &self.config.current_model().provider,
            &self.config.current_model().id,
        )?;
        Ok(self.persist_runtime_state_warning("set_model_resolved"))
    }

    async fn set_thinking(&mut self, requested: &str) -> Result<Option<String>> {
        let level = parse_thinking_level(requested)
            .map_err(|_| UserCommandError::InvalidThinkingLevel(requested.to_string()))?;
        self.config.set_thinking(level);
        self.session
            .inner_mut()
            .append_thinking_change(self.config.current_thinking())?;
        Ok(self.persist_runtime_state_warning("set_thinking"))
    }

    fn current_model_uses_ollama_chat_api(&self) -> bool {
        self.config.current_model().api == ApiKind::OllamaChatApi
    }

    fn context_length_non_ollama_message(&self) -> String {
        let model = self.config.current_model();
        format!(
            "`/context-length` only applies to Ollama native /api/chat models -- selected model '{}:{}' uses {:?}.",
            model.provider, model.id, model.api,
        )
    }

    fn state_summary_message(&self) -> String {
        format_state_summary(
            self.config.current_model(),
            self.config.current_thinking(),
            self.config.active_ollama_num_ctx_override(),
            self.config.anie_config().ollama.default_max_num_ctx,
            self.config.effective_ollama_context_window(),
            self.session.id(),
            anie_config::global_config_path(),
            anie_config::anie_state_json_path(),
            self.compaction_stats.snapshot(),
        )
    }

    fn context_length_status_message(&self) -> String {
        let effective = self.config.effective_ollama_context_window();
        let baseline = self.config.current_model().context_window;
        let cap = self.config.anie_config().ollama.default_max_num_ctx;
        match self.config.active_ollama_num_ctx_override() {
            Some(value) => {
                let mut message = format!(
                    "Current context window: {} (runtime override; baseline {})",
                    format_context_length(effective),
                    format_context_length(baseline),
                );
                // If the user's runtime override exceeds the
                // workspace-wide cap, surface that in the same
                // message so they can see why the wire request
                // might still hit a load failure even with their
                // override active. Cap PR 3.
                if let Some(cap_value) = cap
                    && value > cap_value
                {
                    use std::fmt::Write as _;
                    let _ = write!(
                        message,
                        "; exceeds [ollama] default_max_num_ctx of {}",
                        format_context_length(cap_value)
                    );
                }
                message
            }
            None => match cap {
                Some(_) => format!(
                    "Current context window: {} (workspace cap from [ollama] default_max_num_ctx)",
                    format_context_length(effective),
                ),
                None => format!(
                    "Current context window: {}",
                    format_context_length(effective)
                ),
            },
        }
    }

    fn apply_context_length_argument(
        &mut self,
        argument: &str,
    ) -> Result<ContextLengthMutation, String> {
        if argument.eq_ignore_ascii_case("reset") {
            self.config.clear_ollama_num_ctx_override();
            return Ok(ContextLengthMutation::Reset);
        }

        let value = argument.parse::<u64>().map_err(|_| {
            format!(
                "Invalid context length '{argument}'. Expected an integer from {MIN_OLLAMA_CONTEXT_LENGTH} to {MAX_OLLAMA_CONTEXT_LENGTH}, or reset.",
            )
        })?;
        if !(MIN_OLLAMA_CONTEXT_LENGTH..=MAX_OLLAMA_CONTEXT_LENGTH).contains(&value) {
            return Err(format!(
                "Invalid context length {value}. Expected a value from {MIN_OLLAMA_CONTEXT_LENGTH} to {MAX_OLLAMA_CONTEXT_LENGTH}.",
            ));
        }

        // Above-cap warning (Cap PR 3): the override still
        // applies — user intent wins — but the conflict is
        // surfaced so future load failures aren't a surprise.
        let above_cap_warning = self
            .config
            .anie_config()
            .ollama
            .default_max_num_ctx
            .filter(|cap| value > *cap)
            .map(|cap| {
                format!(
                    "Note: this exceeds [ollama] default_max_num_ctx ({}). The override still applies, but the wire request may hit a load failure on this hardware.",
                    format_context_length(cap),
                )
            });

        self.config.set_ollama_num_ctx_override(value);
        Ok(ContextLengthMutation::Set { above_cap_warning })
    }

    /// Build the compaction config + summarizer for the current
    /// session state. Used by every compaction call site.
    ///
    /// PR 8.1 of `docs/midturn_compaction_2026-04-27/`. The
    /// stored `reserve_tokens` is clamped to a window-relative
    /// fraction here so the resulting threshold lives at
    /// roughly 75% of the window regardless of size. Without
    /// this, a 16K Ollama window minus the default 16K reserve
    /// saturated to threshold 0, and the controller compacted
    /// every turn unconditionally. `anie-session` stays
    /// unaware of windows; the clamp lives at the call site
    /// that builds `CompactionConfig`.
    fn compaction_strategy(
        &self,
        keep_recent_tokens: u64,
    ) -> (CompactionConfig, CompactionStrategy) {
        let context_window = self.config.effective_ollama_context_window();
        let reserve_tokens = crate::compaction_reserve::effective_reserve(
            context_window,
            self.config.anie_config().compaction.reserve_tokens,
            crate::compaction_reserve::DEFAULT_MIN_RESERVE_TOKENS,
        );
        let config = CompactionConfig {
            context_window,
            reserve_tokens,
            keep_recent_tokens,
        };
        let strategy = CompactionStrategy::new(
            self.config.current_model().clone(),
            Arc::clone(&self.provider_registry),
            Arc::clone(&self.request_options_resolver),
            self.config.active_ollama_num_ctx_override(),
        );
        (config, strategy)
    }

    /// Emit the `CompactionEnd` event for a successful compaction.
    /// Callers decide whether to follow with a status refresh or a
    /// transcript replacement, since the ordering matters visually.
    /// `phase` should match the corresponding `CompactionStart`.
    /// Plan 06 of `docs/midturn_compaction_2026-04-27/`.
    async fn emit_compaction_end(
        &self,
        event_tx: &mpsc::Sender<AgentEvent>,
        result: &anie_session::CompactionResult,
        phase: anie_protocol::CompactionPhase,
    ) {
        let tokens_after = self.estimated_context_tokens();
        anie_agent::send_event(
            event_tx,
            AgentEvent::CompactionEnd {
                phase,
                summary: result.summary.clone(),
                tokens_before: result.tokens_before,
                tokens_after,
            },
        )
        .await;
        // Plan 06 PR B: bump the phase counter only after the
        // event has been emitted, so the user-visible event
        // ordering stays unchanged and `/state` reflects the
        // same compactions the transcript shows.
        self.compaction_stats.increment(phase);
    }

    /// Returns `Ok(true)` when a compaction successfully ran,
    /// `Ok(false)` when the threshold wasn't crossed or the
    /// session couldn't be reduced. The caller uses the bool
    /// to decrement the per-turn compaction budget tracked on
    /// `InteractiveController`. Plan
    /// `docs/midturn_compaction_2026-04-27/02_per_turn_compaction_budget.md`.
    async fn maybe_auto_compact(&mut self, event_tx: &mpsc::Sender<AgentEvent>) -> Result<bool> {
        let (config, strategy) =
            self.compaction_strategy(self.config.anie_config().compaction.keep_recent_tokens);

        // Pre-check: if the session isn't past the threshold
        // yet, skip without announcing anything — we don't want
        // "Compacting context…" messages flickering past every
        // turn. When we DO plan to compact, emit the start
        // event BEFORE the (slow) LLM summarization call so the
        // user sees the progress indicator while waiting
        // instead of a silent pause followed by both the start
        // and end messages at once.
        let tokens_before = self.session.inner().estimate_context_tokens();
        let threshold = config.context_window.saturating_sub(config.reserve_tokens);
        if tokens_before <= threshold {
            return Ok(false);
        }
        if !self.session.inner().can_compact(config.keep_recent_tokens) {
            return Ok(false);
        }

        anie_agent::send_event(
            event_tx,
            AgentEvent::CompactionStart {
                phase: anie_protocol::CompactionPhase::PrePrompt,
            },
        )
        .await;

        if let Some(result) = self
            .session
            .inner_mut()
            .auto_compact(&config, &strategy)
            .await?
        {
            self.emit_compaction_end(event_tx, &result, anie_protocol::CompactionPhase::PrePrompt)
                .await;
            anie_agent::send_event(event_tx, self.status_event()).await;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn force_compact(&mut self, event_tx: &mpsc::Sender<AgentEvent>) -> Result<()> {
        let (config, strategy) =
            self.compaction_strategy(self.config.anie_config().compaction.keep_recent_tokens);
        if !self.session.inner().can_compact(config.keep_recent_tokens) {
            anie_agent::send_event(
                event_tx,
                AgentEvent::SystemMessage {
                    text: "Nothing to compact yet.".into(),
                },
            )
            .await;
            return Ok(());
        }

        // Manual `/compact` runs at the prompt boundary; classify
        // it as `PrePrompt` so telemetry treats it the same as the
        // automatic pre-prompt path.
        anie_agent::send_event(
            event_tx,
            AgentEvent::CompactionStart {
                phase: anie_protocol::CompactionPhase::PrePrompt,
            },
        )
        .await;
        match self
            .session
            .inner_mut()
            .force_compact(&config, &strategy)
            .await?
        {
            Some(result) => {
                self.emit_compaction_end(
                    event_tx,
                    &result,
                    anie_protocol::CompactionPhase::PrePrompt,
                )
                .await;
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
        // Plan 06 PR B: counters are session-scoped, so a fresh
        // session zeroes them out alongside the rest of the
        // session-bound state.
        self.compaction_stats.reset();
        self.persist_runtime_state_logged("new_session");
        Ok(())
    }

    async fn switch_session(&mut self, session_id: &str) -> Result<()> {
        self.session
            .switch_to(session_id)
            .map_err(|_| UserCommandError::UnknownSession(session_id.to_string()))?;
        self.apply_session_overrides();
        // Counters are session-scoped (plan 06 PR B). Switching
        // away from the active session zeroes them out so the
        // newly-active session starts with its own count.
        self.compaction_stats.reset();
        self.persist_runtime_state_logged("switch_session");
        Ok(())
    }

    async fn fork_session(&mut self) -> Result<String> {
        let child_id = self.session.fork()?;
        self.apply_session_overrides();
        // Same rationale as `switch_session` — the fork is a
        // distinct session and starts with fresh counters.
        self.compaction_stats.reset();
        self.persist_runtime_state_logged("fork_session");
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

    /// Emit the `RetryScheduled` event and refresh the transcript
    /// so the UI knows a retry is pending.
    ///
    /// No longer sleeps — the backoff is handled by the main
    /// controller loop via `PendingRetry::Armed`. Keeping the
    /// function named `emit_retry_scheduled` rather than reusing
    /// the old name makes it clear at call sites that the sleep
    /// has moved.
    async fn emit_retry_scheduled(
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
        if !self.session.inner().can_compact(config.keep_recent_tokens) {
            anie_agent::send_event(
                event_tx,
                AgentEvent::SystemMessage {
                    text: "Context overflow recovery could not compact the session further.".into(),
                },
            )
            .await;
            return Ok(false);
        }
        anie_agent::send_event(
            event_tx,
            AgentEvent::CompactionStart {
                phase: anie_protocol::CompactionPhase::ReactiveOverflow,
            },
        )
        .await;
        match self
            .session
            .inner_mut()
            .force_compact(&config, &strategy)
            .await?
        {
            Some(result) => {
                self.emit_compaction_end(
                    event_tx,
                    &result,
                    anie_protocol::CompactionPhase::ReactiveOverflow,
                )
                .await;
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

    pub(crate) fn session_context(&self) -> SessionContext {
        self.session.context()
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
            context_window: self.config.effective_ollama_context_window(),
            cwd: self.session.cwd().display().to_string(),
            session_id: self.session.id().to_string(),
        }
    }

    pub(crate) fn model_catalog(&self) -> &[Model] {
        &self.model_catalog
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
        self.persist_runtime_state_logged("reload_config");
        Ok(())
    }

    /// Rebuild the system prompt if the set of context files or any of their mtimes changed.
    fn refresh_system_prompt_if_needed(&mut self) {
        let cwd = self.session.cwd().to_path_buf();
        self.prompt_cache
            .refresh_if_stale(&cwd, &self.tool_registry, self.config.anie_config());
    }
}

/// Drive an agent run through the public REPL step machine.
///
/// Equivalent to calling `AgentLoop::run` directly — both take
/// the same path through `start_run_machine`/`next_step`/`finish`
/// — but the explicit driver shape is the seam future PRs use
/// to interpose step-level policy (queued-prompt folding,
/// proactive compaction, verifier loops). Plan
/// `docs/repl_agent_loop/06_controller_pilot.md`.
async fn run_via_step_machine(
    agent: &AgentLoop,
    prompts: Vec<anie_protocol::Message>,
    context: Vec<anie_protocol::Message>,
    event_tx: &mpsc::Sender<anie_protocol::AgentEvent>,
    cancel: &CancellationToken,
) -> anie_agent::AgentRunResult {
    let mut machine = agent.start_run_machine(prompts, context, event_tx).await;
    while !machine.is_finished() {
        machine.next_step(event_tx, cancel).await;
    }
    machine.finish(event_tx).await
}

fn build_agent(
    state: &ControllerState,
    compaction_gate: Option<Arc<dyn anie_agent::CompactionGate>>,
) -> AgentLoop {
    AgentLoop::new(
        Arc::clone(&state.provider_registry),
        Arc::clone(&state.tool_registry),
        AgentLoopConfig::new(
            state.config.current_model().clone(),
            state.prompt_cache.current().to_string(),
            state.config.current_thinking(),
            ToolExecutionMode::Parallel,
            Arc::clone(&state.request_options_resolver),
        )
        .with_ollama_num_ctx_override(state.config.active_ollama_num_ctx_override())
        .with_compaction_gate(compaction_gate),
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
            "You are an expert coding assistant. You help users by reading files, executing commands, editing code, and writing new files. When web tools are available, you can also answer questions that need information from the live internet — current weather, news, library/package status, documentation lookups, prices, definitions — not just coding research. Don't decline a real-world question on the assumption that your scope is the local project; check the tool list.\n\nAvailable tools:\n{tool_list}\n\nGuidelines:\n- Use bash for file operations like ls, grep, find\n- Use read to examine files (use offset + limit for large files)\n- Use edit for precise changes\n- Use write only for new files or complete rewrites\n- Use web_search + web_read for any question about the live state of the world (weather, news, current events, library docs, prices, etc.) when those tools are available\n- Be concise in your responses"
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
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        _ => Err(format!("invalid thinking level '{value}'")),
    }
}

fn format_thinking(level: ThinkingLevel) -> String {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
    }
    .to_string()
}

fn format_context_length(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn format_state_summary(
    model: &Model,
    thinking: ThinkingLevel,
    runtime_override: Option<u64>,
    workspace_cap: Option<u64>,
    effective: u64,
    session_id: &str,
    config_path: Option<PathBuf>,
    state_path: Option<PathBuf>,
    compaction_stats: CompactionStats,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let _ = writeln!(out, "Current model");
    let _ = writeln!(out, "  {}:{} · {:?}", model.provider, model.id, model.api,);
    let _ = writeln!(out, "  Thinking: {}", format_thinking(thinking));
    let _ = writeln!(out);

    let _ = writeln!(out, "Context window");
    if model.api == ApiKind::OllamaChatApi {
        let suffix = if runtime_override.is_some() {
            " (runtime override active)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "  Effective:        {} tokens{suffix}",
            format_context_length(effective),
        );
        let _ = writeln!(
            out,
            "  Runtime override: {}",
            match runtime_override {
                Some(value) => format!("{} (state.json)", format_context_length(value)),
                None => "(none)".to_string(),
            },
        );
        let _ = writeln!(
            out,
            "  Workspace cap:    {}",
            match workspace_cap {
                Some(value) => format!(
                    "{} (config.toml [ollama] default_max_num_ctx)",
                    format_context_length(value),
                ),
                None => "(none)".to_string(),
            },
        );
        let _ = writeln!(
            out,
            "  Model baseline:   {} (Model.context_window)",
            format_context_length(model.context_window),
        );
    } else {
        let _ = writeln!(
            out,
            "  Effective: {} tokens",
            format_context_length(effective),
        );
        let _ = writeln!(
            out,
            "  (Layered overrides only apply to Ollama /api/chat models)",
        );
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "Session");
    let _ = writeln!(out, "  Active: {session_id}");
    let _ = writeln!(out);

    // Plan 06 PR B of `docs/midturn_compaction_2026-04-27/`.
    // Render the per-session compaction breakdown so users can
    // see how the three phases mixed without trawling logs.
    // anie-specific deviation: counters are this-process-
    // lifetime, not durable. Resuming a session via
    // `--continue` starts the counters at zero — the persisted
    // session log doesn't carry per-phase counts (mid-turn
    // compactions intentionally don't persist; pre-prompt and
    // reactive ones do but without a phase tag in the persisted
    // entry). Backfilling stats from session-log replay is a
    // future plan, tracked as deferred.
    let _ = writeln!(out, "Compactions this session");
    let _ = writeln!(
        out,
        "  Total: {}  (pre-prompt: {}, mid-turn: {}, overflow: {}; this process only)",
        compaction_stats.total(),
        compaction_stats.pre_prompt,
        compaction_stats.mid_turn,
        compaction_stats.reactive_overflow,
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "Files");
    if let Some(path) = config_path {
        let _ = writeln!(out, "  Config: {} (hand-edited)", path.display());
    }
    if let Some(path) = state_path {
        let _ = writeln!(out, "  State:  {} (written by anie)", path.display());
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out
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
