# Onboarding Flow

## Entry points

The onboarding UI can be launched from three places:

1. automatic first-run detection in `anie-cli`
2. `anie onboard`
3. `/onboard` inside the interactive TUI

Provider management is also available from:

- `/providers`
- the onboarding main menu when existing providers are present

## First-run detection

`anie-cli::onboarding::check_first_run()` returns true only when:

- the global config file does not exist
- and `CredentialStore` cannot find any known stored credentials

This prevents first-run onboarding from reappearing after credentials have already been migrated or configured.

## Screen structure

The onboarding UI lives in `crates/anie-tui/src/onboarding.rs`.

Primary states:

- `MainMenu`
- `LocalServerWaiting`
- `LocalServerSelect`
- `ProviderPresetList`
- `ApiKeyInput`
- `CustomEndpoint`
- `Busy`
- `Success`
- `Error`
- `ManagingProviders`

## Background work

The onboarding screen stays synchronous from the event loop's point of view.

Network operations are pushed into Tokio background tasks and report back through an internal `mpsc::UnboundedReceiver`.

This is used for:

- local server detection
- hosted-provider API-key validation
- custom endpoint validation

The same pattern is reused by the provider-management screen for connection testing and API-key edits.

## Config writing

When onboarding completes, configured providers are written with:

- `anie-tui::write_configured_providers()`
- `anie-config::ConfigMutator`

`ConfigMutator` uses `toml_edit::DocumentMut`, so onboarding updates preserve existing comments and formatting in `~/.anie/config.toml`.

## Runtime reload after onboarding

Inside the interactive TUI, the onboarding and provider-management overlays write config locally, then emit:

- `UiAction::ReloadConfig { provider, model }`

The controller reloads config, rebuilds the model catalog, reconstructs the `AuthResolver`, refreshes the system prompt, and publishes a new status event.

This avoids forcing the user to restart `anie` after onboarding changes.
