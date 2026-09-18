use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::mpsc,
};

use anie_protocol::{AgentEvent, CompactionPhase, Message, StopReason, StreamDelta};
use anie_tui::UiAction;

use crate::print_mode::assistant_error_text;
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
    /// Assistant failure carried on `AgentEvent::MessageEnd` when the
    /// message's `stop_reason` is `Error`. Print mode already surfaces
    /// the same text on stderr (`print_mode.rs`, `MessageEnd` arm); the
    /// RPC projection used to blank that event, so a provider give-up
    /// (auth, exhausted retries) was indistinguishable from a
    /// zero-token success on the wire.
    ///
    /// anie-specific (not verified against pi): pi's rpc-mode source
    /// was not available when this landed. The tag is additive on
    /// `hello.version` 1. `error` stays reserved for unparsable stdin.
    #[serde(rename = "assistant_error")]
    AssistantError { message: String },
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
            AgentEvent::MessageEnd {
                message: Message::Assistant(assistant),
            } if matches!(assistant.stop_reason, StopReason::Error) => Self::AssistantError {
                message: assistant_error_text(&assistant).to_string(),
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
    use anie_protocol::{AgentEvent, AssistantMessage, Message, StopReason, Usage, UserMessage};

    use super::*;

    fn assistant_message(stop_reason: StopReason, error_message: Option<&str>) -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            usage: Usage::default(),
            stop_reason,
            error_message: error_message.map(str::to_string),
            provider: "test".into(),
            model: "test-model".into(),
            timestamp: 1,
            reasoning_details: None,
        }
    }

    /// Regression; measured at 18270d9, `anie --rpc` with no API key
    /// answered a prompt with `agent_start`, six `{"type":"system","text":""}`
    /// lines and `agent_end`; the Authentication failed text reached only
    /// the log and the session JSONL.
    #[test]
    fn message_end_with_auth_error_emits_assistant_error_not_empty_system() {
        let event = AgentEvent::MessageEnd {
            message: Message::Assistant(assistant_message(
                StopReason::Error,
                Some("Authentication failed: no key"),
            )),
        };
        assert_eq!(
            serde_json::to_string(&RpcEvent::from(event)).unwrap(),
            r#"{"type":"assistant_error","message":"Authentication failed: no key"}"#
        );
    }

    #[test]
    fn message_end_with_blank_error_message_uses_fallback_sentence() {
        let event = AgentEvent::MessageEnd {
            message: Message::Assistant(assistant_message(StopReason::Error, Some("   "))),
        };
        assert_eq!(
            serde_json::to_string(&RpcEvent::from(event)).unwrap(),
            r#"{"type":"assistant_error","message":"assistant response ended with an error"}"#
        );
    }

    #[test]
    fn message_end_user_role_still_emits_empty_system() {
        let event = AgentEvent::MessageEnd {
            message: Message::User(UserMessage {
                content: Vec::new(),
                timestamp: 1,
            }),
        };
        assert_eq!(
            serde_json::to_string(&RpcEvent::from(event)).unwrap(),
            r#"{"type":"system","text":""}"#
        );
    }

    #[test]
    fn message_end_with_stop_reason_stop_still_emits_empty_system() {
        let event = AgentEvent::MessageEnd {
            message: Message::Assistant(assistant_message(StopReason::Stop, None)),
        };
        assert_eq!(
            serde_json::to_string(&RpcEvent::from(event)).unwrap(),
            r#"{"type":"system","text":""}"#
        );
    }

    #[test]
    fn message_end_aborted_does_not_emit_assistant_error() {
        let event = AgentEvent::MessageEnd {
            message: Message::Assistant(assistant_message(StopReason::Aborted, None)),
        };
        assert_eq!(
            serde_json::to_string(&RpcEvent::from(event)).unwrap(),
            r#"{"type":"system","text":""}"#
        );
    }
}
