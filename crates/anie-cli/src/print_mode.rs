use std::io::Write;

use anyhow::{Context, Result, anyhow};
use tokio::sync::mpsc;

use anie_protocol::{AgentEvent, ContentBlock, Message, StopReason, StreamDelta};
use anie_tui::UiAction;

use crate::{Cli, bootstrap::prepare_controller_state, controller::InteractiveController};

/// Start one-shot print mode.
pub(crate) async fn run_print_mode(cli: Cli) -> Result<()> {
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
