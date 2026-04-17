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
- `DiscoveringModels`
- `PickingModel`
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
- model discovery for onboarding pickers

The same pattern is reused by the provider-management screen for connection testing, API-key edits, and model discovery.

## Config writing

When onboarding completes, configured providers are written with:

- `anie-tui::write_configured_providers()`
- `anie-config::ConfigMutator`
- `anie-config::preferred_write_target()`

`ConfigMutator` uses `toml_edit::DocumentMut`, so onboarding updates preserve existing comments and formatting in the chosen config target (`~/.anie/config.toml` or the nearest project `.anie/config.toml`).

## Runtime reload after onboarding

Inside the interactive TUI, the onboarding and provider-management overlays write config locally, then emit:

- `UiAction::ReloadConfig { provider, model }`

The controller reloads config, rebuilds the model catalog, reconstructs the `AuthResolver`, refreshes the system prompt, and publishes a new status event.

## Inline model selection

Each onboarding path now validates the provider first and then runs model discovery before finalizing configuration:

- **Local server** → detect server → discover models → pick inside the onboarding card
- **API preset** → validate API key → discover models → pick inside the onboarding card
- **Custom endpoint** → validate endpoint → discover models → pick inside the onboarding card, with manual model-ID fallback when discovery fails

The shared picker is the same search-first component used by `/model` and provider management.

This avoids forcing the user to restart `anie` after onboarding changes.
