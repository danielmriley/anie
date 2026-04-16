//! CLI entry points for interactive, print, and RPC modes.

mod controller;
mod onboarding;
mod runtime_state;

use std::{path::PathBuf, sync::OnceLock};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::{info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

pub use controller::build_system_prompt;

static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Main CLI arguments.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "anie",
    version,
    about = "A coding agent harness",
    subcommand_precedence_over_arg = true
)]
pub struct Cli {
    /// Optional command entry point.
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Run in interactive TUI mode.
    #[arg(short, long)]
    pub interactive: bool,
    /// Run in one-shot print mode.
    #[arg(short, long)]
    pub print: bool,
    /// Run in RPC mode (JSONL over stdin/stdout).
    #[arg(long)]
    pub rpc: bool,
    /// Disable tool registration.
    #[arg(long)]
    pub no_tools: bool,
    /// Initial prompt.
    #[arg(trailing_var_arg = true)]
    pub prompt: Vec<String>,
    /// Override the selected model ID.
    #[arg(long)]
    pub model: Option<String>,
    /// Override the selected provider.
    #[arg(long)]
    pub provider: Option<String>,
    /// Override the API key used for the request.
    #[arg(long)]
    pub api_key: Option<String>,
    /// Override the thinking level.
    #[arg(long, value_parser = controller::parse_thinking_level)]
    pub thinking: Option<anie_provider::ThinkingLevel>,
    /// Resume a previous session by ID.
    #[arg(long)]
    pub resume: Option<String>,
    /// Override the working directory.
    #[arg(short = 'C', long)]
    pub cwd: Option<PathBuf>,
}

/// Supported top-level subcommands.
#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Launch the interactive onboarding flow.
    Onboard,
}

/// Run the CLI entry point.
pub async fn run(cli: Cli) -> Result<()> {
    init_tracing();

    if let Some(cwd) = &cli.cwd {
        std::env::set_current_dir(cwd)
            .with_context(|| format!("failed to change directory to {}", cwd.display()))?;
    }

    if matches!(cli.command, Some(Command::Onboard)) {
        return onboarding::run_onboarding().await;
    }

    let credential_store = anie_auth::CredentialStore::new();
    if credential_store.should_migrate() {
        match credential_store.migrate_from_json() {
            Ok(0) => {}
            Ok(count) => info!(count, "migrated credentials into native keyring"),
            Err(error) => warn!(%error, "credential migration skipped"),
        }
    }

    if onboarding::check_first_run() && !cli.rpc {
        onboarding::run_onboarding().await?;
    }

    if cli.rpc {
        controller::run_rpc_mode(cli).await
    } else if cli.print || !cli.prompt.is_empty() {
        controller::run_print_mode(cli).await
    } else {
        controller::run_interactive_mode(cli).await
    }
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "anie=info".into());

    if let Some(log_dir) = dirs::home_dir().map(|home| home.join(".anie/logs")) {
        if std::fs::create_dir_all(&log_dir).is_ok() {
            let file_appender = tracing_appender::rolling::daily(&log_dir, "anie.log");
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
            let _ = LOG_GUARD.set(guard);
            let _ = tracing_subscriber::registry()
                .with(env_filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(non_blocking)
                        .with_ansi(false),
                )
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_onboard_subcommand() {
        let cli = Cli::parse_from(["anie", "onboard"]);
        assert_eq!(cli.command, Some(Command::Onboard));
        assert!(cli.prompt.is_empty());
    }

    #[test]
    fn positional_prompt_still_parses_without_subcommand() {
        let cli = Cli::parse_from(["anie", "hello world"]);
        assert_eq!(cli.command, None);
        assert_eq!(cli.prompt, vec!["hello world".to_string()]);
    }

    #[test]
    fn prompt_and_model_flags_still_parse() {
        let cli = Cli::parse_from(["anie", "--model", "gpt-4o", "hello"]);
        assert_eq!(cli.command, None);
        assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
        assert_eq!(cli.prompt, vec!["hello".to_string()]);
    }
}
