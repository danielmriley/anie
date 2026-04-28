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
    let (ui_action_tx, ui_action_rx) = mpsc::unbounded_channel();
    let controller = InteractiveController::new(state, ui_action_rx, agent_event_tx, true);
    let controller_task = tokio::spawn(async move { controller.run().await });

    let abort_tx = ui_action_tx.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = abort_tx.send(UiAction::Abort);
    });

    ui_action_tx
        .send(UiAction::SubmitPrompt(prompt))
        .context("failed to start print-mode prompt")?;

    let mut streamed_text = false;
    let mut printed_assistant_output = false;
    let mut pending_terminal_text: Option<String> = None;
    process_print_events(
        &mut agent_event_rx,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        &mut streamed_text,
        &mut printed_assistant_output,
        &mut pending_terminal_text,
    )
    .await?;

    let _ = ui_action_tx.send(UiAction::Quit);
    match controller_task.await {
        Ok(result) => result,
        Err(error) => Err(anyhow!("print-mode controller task failed: {error}")),
    }
}

async fn process_print_events(
    agent_event_rx: &mut mpsc::Receiver<AgentEvent>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    streamed_text: &mut bool,
    printed_assistant_output: &mut bool,
    pending_terminal_text: &mut Option<String>,
) -> Result<()> {
    while let Some(event) = agent_event_rx.recv().await {
        match event {
            AgentEvent::MessageStart {
                message: Message::Assistant(_),
            } => {
                *streamed_text = false;
                *pending_terminal_text = None;
            }
            AgentEvent::MessageDelta {
                delta: StreamDelta::TextDelta(text),
            } => {
                write!(stdout, "{text}").context("failed to write stdout")?;
                stdout.flush().context("failed to flush stdout")?;
                *streamed_text = true;
                *printed_assistant_output = true;
                *pending_terminal_text = None;
            }
            AgentEvent::MessageEnd {
                message: Message::Assistant(assistant),
            } => {
                if matches!(assistant.stop_reason, StopReason::Error) {
                    if *streamed_text {
                        writeln!(stderr, "\n[error: {}]", assistant_error_text(&assistant))
                            .context("failed to write stderr")?;
                    } else {
                        *pending_terminal_text = Some(assistant_text(&assistant.content));
                    }
                } else if !*streamed_text {
                    let text = assistant_text(&assistant.content);
                    if !text.is_empty() {
                        write!(stdout, "{text}").context("failed to write stdout")?;
                        stdout.flush().context("failed to flush stdout")?;
                        *printed_assistant_output = true;
                    }
                }
            }
            AgentEvent::ToolExecStart {
                tool_name, args, ..
            } => {
                writeln!(stderr, "\n[tool: {tool_name} {}]", tool_hint(&args))
                    .context("failed to write stderr")?;
            }
            AgentEvent::ToolExecEnd { is_error, .. } if is_error => {
                writeln!(stderr, "[tool error]").context("failed to write stderr")?;
            }
            AgentEvent::SystemMessage { text } => {
                writeln!(stderr, "\n{text}").context("failed to write stderr")?;
            }
            AgentEvent::CompactionStart { phase: _ } => {
                // Non-interactive print mode treats every
                // compaction phase the same — the user is
                // looking at stderr, not at a live activity
                // row, so the marker just confirms a
                // compaction is happening. PR C of plan 06
                // surfaces the phase label in the TUI; print
                // mode doesn't need it.
                writeln!(stderr, "\n[compacting context]").context("failed to write stderr")?;
            }
            AgentEvent::CompactionEnd {
                tokens_before,
                tokens_after,
                ..
            } => {
                writeln!(
                    stderr,
                    "\n[compaction complete: {} -> {}]",
                    format_tokens(tokens_before),
                    format_tokens(tokens_after)
                )
                .context("failed to write stderr")?;
            }
            AgentEvent::RetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => {
                *pending_terminal_text = None;
                writeln!(
                    stderr,
                    "\n[retrying {}/{} in {:.1}s: {}]",
                    attempt,
                    max_retries,
                    delay_ms as f64 / 1000.0,
                    error,
                )
                .context("failed to write stderr")?;
            }
            AgentEvent::TranscriptReplace { .. } => {
                *pending_terminal_text = None;
                *streamed_text = false;
            }
            AgentEvent::AgentEnd { messages } => {
                if !*printed_assistant_output
                    && let Some(Message::Assistant(assistant)) = messages.last()
                    && !matches!(assistant.stop_reason, StopReason::Error)
                {
                    let text = assistant_text(&assistant.content);
                    if !text.is_empty() {
                        write!(stdout, "{text}").context("failed to write stdout")?;
                        stdout.flush().context("failed to flush stdout")?;
                        *printed_assistant_output = true;
                    }
                }
            }
            _ => {}
        }
    }

    if !*printed_assistant_output
        && let Some(text) = pending_terminal_text.take()
        && !text.is_empty()
    {
        write!(stdout, "{text}").context("failed to write stdout")?;
        stdout.flush().context("failed to flush stdout")?;
    }
    writeln!(stdout).context("failed to write stdout")?;
    Ok(())
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
    // Plan 08 PR-A: direct-buffer join — skip the intermediate
    // Vec<&str>. Sizing pass + write pass, one allocation.
    let mut total = 0usize;
    let mut first = true;
    for block in content {
        if let ContentBlock::Text { text } = block {
            if !first {
                total += 1;
            }
            total += text.len();
            first = false;
        }
    }
    let mut out = String::with_capacity(total);
    let mut first = true;
    for block in content {
        if let ContentBlock::Text { text } = block {
            if !first {
                out.push('\n');
            }
            out.push_str(text);
            first = false;
        }
    }
    out
}

fn assistant_error_text(assistant: &anie_protocol::AssistantMessage) -> &str {
    assistant
        .error_message
        .as_deref()
        .filter(|message| !message.trim().is_empty())
        .unwrap_or("assistant response ended with an error")
}

#[cfg(test)]
mod tests {
    use anie_protocol::{AssistantMessage, Usage};

    use super::*;

    fn assistant_message(
        stop_reason: StopReason,
        error_message: Option<&str>,
        text: &str,
    ) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            usage: Usage::default(),
            stop_reason,
            error_message: error_message.map(str::to_string),
            provider: "test".into(),
            model: "test-model".into(),
            timestamp: 1,
            reasoning_details: None,
        })
    }

    async fn process_events(events: Vec<AgentEvent>) -> (String, String) {
        let (tx, mut rx) = mpsc::channel(events.len().max(1));
        for event in events {
            tx.send(event).await.expect("send event");
        }
        drop(tx);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut streamed_text = false;
        let mut printed_assistant_output = false;
        let mut pending_terminal_text = None;

        process_print_events(
            &mut rx,
            &mut stdout,
            &mut stderr,
            &mut streamed_text,
            &mut printed_assistant_output,
            &mut pending_terminal_text,
        )
        .await
        .expect("process events");

        (
            String::from_utf8(stdout).expect("stdout utf8"),
            String::from_utf8(stderr).expect("stderr utf8"),
        )
    }

    #[tokio::test]
    async fn streamed_success_stdout_is_unchanged() {
        let (stdout, stderr) = process_events(vec![
            AgentEvent::MessageStart {
                message: assistant_message(StopReason::Stop, None, ""),
            },
            AgentEvent::MessageDelta {
                delta: StreamDelta::TextDelta("hello".into()),
            },
            AgentEvent::MessageEnd {
                message: assistant_message(StopReason::Stop, None, "hello"),
            },
        ])
        .await;

        assert_eq!(stdout, "hello\n");
        assert_eq!(stderr, "");
    }

    #[tokio::test]
    async fn streamed_partial_output_followed_by_error_prints_stderr_marker() {
        let (stdout, stderr) = process_events(vec![
            AgentEvent::MessageStart {
                message: assistant_message(StopReason::Stop, None, ""),
            },
            AgentEvent::MessageDelta {
                delta: StreamDelta::TextDelta("partial".into()),
            },
            AgentEvent::MessageEnd {
                message: assistant_message(
                    StopReason::Error,
                    Some("provider timed out"),
                    "partial",
                ),
            },
        ])
        .await;

        assert_eq!(stdout, "partial\n");
        assert_eq!(stderr, "\n[error: provider timed out]\n");
    }
}
