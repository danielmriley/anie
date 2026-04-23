use anyhow::{Result, anyhow};
use tokio::sync::mpsc;

use anie_protocol::AgentEvent;
use anie_tui::{App, TerminalCapabilities, install_panic_hook, run_tui, setup_terminal};

use crate::{
    Cli,
    bootstrap::{prepare_controller_state, spawn_shutdown_signal_forwarder},
};

/// Start the full interactive TUI mode.
pub(crate) async fn run_interactive_mode(cli: Cli) -> Result<()> {
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
    // Unbounded so user actions (submit, quit, abort) can never
    // be silently dropped when the controller is busy. Producer is
    // a single TUI task at human-typing speed; memory growth is
    // not a real risk. See plan 13 phase B.
    let (ui_action_tx, ui_action_rx) = mpsc::unbounded_channel();
    spawn_shutdown_signal_forwarder(ui_action_tx.clone());

    let initial_models = state.model_catalog().to_vec();
    let initial_commands = state.command_registry.all().to_vec();
    let popup_enabled = state.config.anie_config().ui.slash_command_popup_enabled;
    let markdown_enabled = state.config.anie_config().ui.markdown_enabled;
    let tool_output_mode = state.config.anie_config().ui.tool_output_mode;
    let capabilities = TerminalCapabilities::detect();
    let controller =
        crate::controller::InteractiveController::new(state, ui_action_rx, agent_event_tx, false);
    let controller_task = tokio::spawn(async move { controller.run().await });

    let mut app = App::new(agent_event_rx, ui_action_tx, initial_models, initial_commands)
        .with_autocomplete_enabled(popup_enabled)
        .with_markdown_enabled(markdown_enabled)
        .with_tool_output_mode(tool_output_mode)
        .with_terminal_capabilities(capabilities);
    apply_status_event(app.status_bar_mut(), &initial_status);
    app.load_transcript(&transcript);

    // The guard enters raw mode + alternate screen + mouse
    // capture. On any exit path — normal return, `?` early
    // return, or panic unwind — Drop restores the terminal, so
    // the shell never ends up with leftover SGR mouse-tracking
    // sequences firing on clicks/scrolls.
    let mut terminal_guard = setup_terminal()?;
    let run_result = run_tui(terminal_guard.terminal_mut(), &mut app).await;
    // Explicit restore to surface any error; the Drop that
    // follows is a no-op. If this fails (exceedingly rare),
    // Drop will still swallow a retry best-effort.
    terminal_guard.restore()?;

    match controller_task.await {
        Ok(controller_result) => controller_result?,
        Err(error) => return Err(anyhow!("interactive controller task failed: {error}")),
    }

    run_result
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
