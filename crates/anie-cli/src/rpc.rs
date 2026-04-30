use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::mpsc,
};

use anie_protocol::{AgentEvent, CompactionPhase, Message, StreamDelta};
use anie_tui::UiAction;

use crate::{Cli, bootstrap::prepare_controller_state, controller::InteractiveController};

/// Start minimal JSONL RPC mode.
pub(crate) async fn run_rpc_mode(cli: Cli) -> Result<()> {
    let state = prepare_controller_state(&cli).await?;
    let (agent_event_tx, agent_event_rx) = mpsc::channel(256);
    let (ui_action_tx, ui_action_rx) = mpsc::unbounded_channel();
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
                if ui_action_tx.send(action).is_err() {
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

/// Wire-format mirror of `anie_protocol::CompactionPhase`,
/// kept separate so the snake-case JSON variants stay stable
/// even if the in-process enum is renamed. Plan 06 of
/// `docs/midturn_compaction_2026-04-27/`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum RpcCompactionPhase {
    PrePrompt,
    MidTurn,
    ReactiveOverflow,
}

impl From<CompactionPhase> for RpcCompactionPhase {
    fn from(value: CompactionPhase) -> Self {
        match value {
            CompactionPhase::PrePrompt => Self::PrePrompt,
            CompactionPhase::MidTurn => Self::MidTurn,
            CompactionPhase::ReactiveOverflow => Self::ReactiveOverflow,
        }
    }
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
    /// rlm policy fired and updated its external archive.
    /// Additive on the wire — RPC consumers that don't
    /// care about rlm internals can ignore this event.
    #[serde(rename = "rlm_stats")]
    RlmStats { archived_messages: u64 },
    #[serde(rename = "status")]
    Status {
        provider: String,
        model: String,
        thinking: String,
        estimated_context_tokens: u64,
        context_window: u64,
        cwd: String,
        session_id: String,
        /// Harness-mode label ("current" | "baseline" | "rlm").
        /// Additive on the wire — existing consumers that
        /// don't unpack this field are unaffected.
        harness_mode: String,
        /// rlm policy's external archive size. Additive on
        /// the wire; 0 outside rlm mode.
        rlm_archived_messages: u64,
    },
    #[serde(rename = "compaction_start")]
    CompactionStart {
        /// One of `pre_prompt`, `mid_turn`, `reactive_overflow`
        /// per `CompactionPhase`. Plan 06 of
        /// `docs/midturn_compaction_2026-04-27/`.
        phase: RpcCompactionPhase,
    },
    #[serde(rename = "compaction_end")]
    CompactionEnd {
        phase: RpcCompactionPhase,
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
            AgentEvent::RlmStatsUpdate { archived_messages } => {
                Self::RlmStats { archived_messages }
            }
            AgentEvent::StatusUpdate {
                provider,
                model_name,
                thinking,
                estimated_context_tokens,
                context_window,
                cwd,
                session_id,
                harness_mode,
                rlm_archived_messages,
            } => Self::Status {
                provider,
                model: model_name,
                thinking,
                estimated_context_tokens,
                context_window,
                cwd,
                session_id,
                harness_mode,
                rlm_archived_messages,
            },
            AgentEvent::CompactionStart { phase } => Self::CompactionStart {
                phase: phase.into(),
            },
            AgentEvent::CompactionEnd {
                phase,
                summary,
                tokens_before,
                tokens_after,
            } => Self::CompactionEnd {
                phase: phase.into(),
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
