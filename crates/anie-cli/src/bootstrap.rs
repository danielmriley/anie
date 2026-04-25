use std::{path::Path, sync::Arc};

use anyhow::{Context, Result};

use anie_auth::AuthResolver;
use anie_config::{CliOverrides, load_config};
use anie_provider::{ProviderRegistry, RequestOptionsResolver};
use anie_providers_builtin::register_builtin_providers;
use anie_session::SessionManager;
use anie_tools::{
    BashPolicy, BashTool, EditTool, FileMutationQueue, FindTool, GrepTool, LsTool, ReadTool,
    WriteTool,
};
use anie_tui::UiAction;
use tracing::warn;

use crate::{
    Cli,
    controller::ControllerState,
    model_catalog::{build_model_catalog, resolve_initial_selection},
    retry_policy::RetryConfig,
    runtime::{ConfigState, SessionHandle, SystemPromptCache},
    runtime_state::load_runtime_state,
};
use anie_agent::ToolRegistry;

pub(crate) async fn prepare_controller_state(cli: &Cli) -> Result<ControllerState> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let config = load_config(CliOverrides::default())?;
    let runtime_state = load_runtime_state().unwrap_or_default();

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

    let tool_registry = build_tool_registry_with_policy(
        &cwd,
        cli.no_tools,
        bash_policy_from_config(&config.tools.bash.policy),
    );
    let prompt_cache = SystemPromptCache::build(&cwd, &tool_registry, &config)?;
    let request_options_resolver: Arc<dyn RequestOptionsResolver> =
        Arc::new(AuthResolver::new(cli.api_key.clone(), config.clone()));

    let mut state = ControllerState {
        config: ConfigState::new(
            config,
            runtime_state,
            selection.model,
            selection.thinking,
            cli.api_key.clone(),
        ),
        session: SessionHandle::from_manager(session, sessions_dir, cwd),
        model_catalog,
        provider_registry,
        tool_registry,
        request_options_resolver,
        prompt_cache,
        retry_config: RetryConfig::default(),
        command_registry: crate::commands::CommandRegistry::with_builtins(),
    };
    state.apply_session_overrides();
    if let Err(error) = state.persist_runtime_state() {
        warn!(%error, "failed to persist runtime state during bootstrap");
    }
    Ok(state)
}

#[cfg(test)]
pub(crate) fn build_tool_registry(cwd: &Path, no_tools: bool) -> Arc<ToolRegistry> {
    build_tool_registry_with_policy(cwd, no_tools, BashPolicy::default())
}

fn build_tool_registry_with_policy(
    cwd: &Path,
    no_tools: bool,
    bash_policy: BashPolicy,
) -> Arc<ToolRegistry> {
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
    tools.register(Arc::new(BashTool::with_policy(
        cwd.to_path_buf(),
        bash_policy,
    )));
    tools.register(Arc::new(GrepTool::new(cwd.to_path_buf())));
    tools.register(Arc::new(FindTool::new(cwd.to_path_buf())));
    tools.register(Arc::new(LsTool::new(cwd.to_path_buf())));
    Arc::new(tools)
}

fn bash_policy_from_config(config: &anie_config::BashPolicyConfig) -> BashPolicy {
    BashPolicy {
        enabled: config.enabled,
        deny_commands: config.deny_commands.clone(),
        deny_patterns: config.deny_patterns.clone(),
    }
}

pub(crate) fn spawn_shutdown_signal_forwarder(
    action_tx: tokio::sync::mpsc::UnboundedSender<UiAction>,
) {
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
                    let _ = action_tx.send(UiAction::Quit);
                }
                _ = sighup.recv() => {
                    let _ = action_tx.send(UiAction::Quit);
                }
            }
        });
    }
}
