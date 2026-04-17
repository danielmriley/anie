use std::{path::Path, time::Duration};

use anyhow::{Context, Result};
use crossterm::event::Event;
use crossterm::event::EventStream;
use futures::StreamExt;

use anie_auth::CredentialStore;
use anie_config::{global_config_path, preferred_write_target};
use anie_tui::{
    OnboardingAction, OnboardingCompletion, OnboardingScreen, install_panic_hook, restore_terminal,
    setup_terminal, write_configured_providers,
};

/// Detect whether this looks like a first run with no config or saved credentials.
#[must_use]
pub fn check_first_run() -> bool {
    is_first_run_with(global_config_path().as_deref(), &CredentialStore::new())
}

fn is_first_run_with(config_path: Option<&Path>, credential_store: &CredentialStore) -> bool {
    if config_path.is_some_and(Path::exists) {
        return false;
    }

    credential_store.list_providers().is_empty()
}

/// Run the full-screen onboarding flow.
pub async fn run_onboarding() -> Result<()> {
    install_panic_hook();

    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let config_path = preferred_write_target(&cwd).context("home directory is not available")?;
    let mut screen = OnboardingScreen::new(CredentialStore::new());
    let mut terminal = setup_terminal()?;
    let mut events = EventStream::new();

    let result: Result<Option<OnboardingCompletion>> = loop {
        terminal.draw(|frame| screen.render(frame, frame.area()))?;

        tokio::select! {
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    match screen.handle_key(key) {
                        OnboardingAction::Continue => {}
                        OnboardingAction::Cancelled => break Ok(None),
                        OnboardingAction::Complete(completion) => break Ok(Some(completion)),
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                match screen.handle_tick() {
                    OnboardingAction::Continue => {}
                    OnboardingAction::Cancelled => break Ok(None),
                    OnboardingAction::Complete(completion) => break Ok(Some(completion)),
                }
            }
        }
    };

    restore_terminal(&mut terminal)?;

    let Some(completion) = result? else {
        println!("Onboarding cancelled.");
        return Ok(());
    };
    if completion.providers.is_empty() {
        match completion.reload_target {
            Some((provider, model)) => {
                println!(
                    "Saved provider-management changes{}{}.",
                    provider
                        .as_deref()
                        .map(|value| format!(" for {value}"))
                        .unwrap_or_default(),
                    model
                        .as_deref()
                        .map(|value| format!(":{value}"))
                        .unwrap_or_default(),
                );
            }
            None => {
                println!("Onboarding finished with no configuration changes.");
            }
        }
        return Ok(());
    }

    match write_configured_providers(&config_path, &completion.providers)? {
        Some((provider, model)) => {
            println!(
                "Saved onboarding configuration to {} using {provider}:{model}.",
                config_path.display()
            );
        }
        None => {
            println!("Onboarding finished with no configuration changes.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn first_run_requires_missing_config_and_missing_credentials() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let credential_store =
            CredentialStore::with_config("anie-test", Some(dir.path().join("auth.json")))
                .without_native_keyring();

        assert!(is_first_run_with(Some(&config_path), &credential_store));
    }

    #[test]
    fn existing_config_disables_first_run() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[model]\nprovider = \"openai\"\nid = \"gpt-4o\"\n",
        )
        .expect("write config");
        let credential_store =
            CredentialStore::with_config("anie-test", Some(dir.path().join("auth.json")))
                .without_native_keyring();

        assert!(!is_first_run_with(Some(&config_path), &credential_store));
    }

    #[test]
    fn existing_credentials_disable_first_run() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let credential_store =
            CredentialStore::with_config("anie-test", Some(dir.path().join("auth.json")))
                .without_native_keyring();
        credential_store
            .set("openai", "sk-test")
            .expect("save credential");

        assert!(!is_first_run_with(Some(&config_path), &credential_store));
    }
}
