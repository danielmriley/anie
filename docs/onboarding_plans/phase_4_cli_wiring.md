# Phase 4 — CLI Wiring and Slash Command

This phase connects the `OnboardingScreen` (Phase 3) into the application.  It wires the `anie onboard` subcommand, the `/onboard` slash command inside the running TUI, and updates the first-run detection logic.

## Why this phase exists

The `OnboardingScreen` widget exists but is not reachable by users.  This phase makes it accessible through three entry points:

1. **Automatic first-run** — when no providers are configured, the TUI onboarding screen launches immediately.
2. **`anie onboard`** — a standalone subcommand that launches the onboarding TUI.
3. **`/onboard`** — a slash command inside the running agent TUI.

---

## Files expected to change

### Primary

- `crates/anie-cli/src/lib.rs` — add `Onboard` subcommand variant; update first-run logic.
- `crates/anie-cli/src/onboarding.rs` — replace stdio-based flow with `OnboardingScreen` launcher.
- `crates/anie-tui/src/app.rs` — add `/onboard` slash command handling; add modal overlay mode for the onboarding screen.
- `crates/anie-tui/src/lib.rs` — re-export any needed types.

### Secondary

- `crates/anie-cli/src/controller.rs` — handle `UiAction::RunOnboarding` if onboarding triggers model/provider changes at runtime.

### Deleted or substantially rewritten

- The body of `crates/anie-cli/src/onboarding.rs` — the old stdio code is replaced.

---

## Sub-steps

### Sub-step A — Add `anie onboard` subcommand

Update `crates/anie-cli/src/lib.rs`:

```rust
#[derive(Debug, Clone, Parser)]
#[command(name = "anie", version, about = "A coding agent harness")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    // ... existing fields ...
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum Command {
    /// Launch the interactive onboarding experience.
    Onboard,
}
```

In `run()`:

```rust
pub async fn run(cli: Cli) -> Result<()> {
    init_tracing();
    // ...

    if let Some(Command::Onboard) = &cli.command {
        return onboarding::run_onboarding_tui().await;
    }

    // ... existing first-run check and mode dispatch ...
}
```

**Design note:** If the existing `Cli` struct uses positional args for the prompt (`trailing_var_arg`), adding a subcommand requires careful clap configuration to avoid breaking `anie "prompt text"` usage.  Use `#[command(subcommand_negates_reqs = true)]` or restructure as needed.  Test both `anie onboard` and `anie "hello world"` to verify.

### Sub-step B — Replace stdio onboarding with TUI launcher

Rewrite `crates/anie-cli/src/onboarding.rs`:

```rust
use anie_auth::CredentialStore;
use anie_tui::OnboardingScreen;

/// Detect whether this looks like a first run with no providers configured.
pub fn check_first_run() -> bool {
    let credential_store = CredentialStore::new();
    let config_path = anie_config::global_config_path();

    // First run if: no config file AND no credentials stored
    config_path.as_deref().is_some_and(|path| !path.exists())
        && credential_store.list_providers().is_empty()
}

/// Launch the full-screen onboarding TUI.
pub async fn run_onboarding_tui() -> Result<()> {
    let credential_store = CredentialStore::new();
    let mut screen = OnboardingScreen::new(credential_store);

    // Set up terminal (similar to App::run but simpler)
    let mut terminal = setup_terminal()?;

    loop {
        terminal.draw(|frame| {
            screen.render(frame, frame.area());
        })?;

        // Handle events
        if let Some(event) = poll_event()? {
            match event {
                Event::Key(key) => {
                    match screen.handle_key(key) {
                        OnboardingAction::Complete(providers) => {
                            restore_terminal(&mut terminal)?;
                            write_config_from_providers(&providers)?;
                            return Ok(());
                        }
                        OnboardingAction::Cancelled => {
                            restore_terminal(&mut terminal)?;
                            return Ok(());
                        }
                        OnboardingAction::Continue => {}
                    }
                }
                _ => {}
            }
        }

        // Tick for spinners and async polling
        match screen.handle_tick() {
            OnboardingAction::Complete(providers) => {
                restore_terminal(&mut terminal)?;
                write_config_from_providers(&providers)?;
                return Ok(());
            }
            _ => {}
        }
    }
}

fn write_config_from_providers(providers: &[ConfiguredProvider]) -> Result<()> {
    // Generate config.toml content from ConfiguredProvider list
    // Write to ~/.anie/config.toml
    // Similar to existing detected_local_config() / provider_config() helpers
}
```

### Sub-step C — Update first-run detection

The current `check_first_run()` checks for config file and auth file existence.  Update it to also consult `CredentialStore::list_providers()`:

```rust
pub fn check_first_run() -> bool {
    let config_exists = anie_config::global_config_path()
        .as_deref()
        .is_some_and(|path| path.exists());
    if config_exists {
        return false;
    }

    let credential_store = CredentialStore::new();
    credential_store.list_providers().is_empty()
}
```

This means:
- If a config file exists → not first run (even if no credentials).
- If no config file but credentials exist in keyring → not first run.
- If neither → first run → launch onboarding.

### Sub-step D — Add `/onboard` slash command to the TUI

In `crates/anie-tui/src/app.rs`, add handling for the `/onboard` command:

1. Add a new `UiAction` variant:

```rust
pub enum UiAction {
    // ... existing variants ...
    /// Launch the onboarding screen.
    RunOnboarding,
}
```

2. In the slash command parsing (wherever `/compact`, `/tools`, etc. are handled):

```rust
"/onboard" => Some(UiAction::RunOnboarding),
```

3. Add a modal overlay mode to `App`:

```rust
pub struct App {
    // ... existing fields ...
    onboarding_overlay: Option<OnboardingScreen>,
}
```

When `onboarding_overlay` is `Some(...)`:
- All key events are routed to the overlay's `handle_key()`.
- The overlay renders on top of the main UI (full-screen or centered modal).
- `handle_tick()` is called on each tick.
- On `OnboardingAction::Complete` → apply changes, close overlay, emit relevant actions to controller.
- On `OnboardingAction::Cancelled` → close overlay.

4. In the render loop:

```rust
fn render(&self, frame: &mut Frame) {
    // ... render main UI ...

    if let Some(onboarding) = &self.onboarding_overlay {
        // Render as full-screen overlay
        onboarding.render(frame, frame.area());
    }
}
```

### Sub-step E — Handle post-onboarding config reload

When onboarding completes inside the running TUI (via `/onboard`), the controller needs to pick up the new provider/model configuration:

1. `App` emits `UiAction::RunOnboarding` when the slash command is entered.
2. The controller opens the onboarding overlay (or the `App` handles it internally).
3. On completion, the controller:
   - Reloads `AnieConfig`.
   - Rebuilds the `AuthResolver` with the new `CredentialStore`.
   - Updates the active model if the default changed.
   - Updates the status bar.

This may require a new `UiAction::ConfigReloaded` or the controller can simply re-initialize its config after receiving the completion signal.

### Sub-step F — Remove old stdio onboarding code

Delete the following from `crates/anie-cli/src/onboarding.rs`:
- `configure_builtin_provider()`
- `configure_custom_provider()`
- `detected_local_config()`
- `custom_provider_config()`
- `provider_config()`

These are replaced by the TUI flow and `write_config_from_providers()`.

Keep `check_first_run()` (updated) and the config-writing helpers.

### Sub-step G — Update tests

1. **First-run detection tests**:
   - No config + no credentials → `check_first_run()` returns `true`.
   - Config exists → returns `false`.
   - No config but credentials exist → returns `false`.

2. **Subcommand parsing tests**:
   - `anie onboard` parses to `Command::Onboard`.
   - `anie "hello"` still works as a prompt.
   - `anie --model gpt-4o "hello"` still works.

3. **Slash command tests**:
   - `/onboard` emits `UiAction::RunOnboarding`.
   - Overlay opens and captures input.
   - Overlay close restores normal input handling.

---

## Constraints

1. **`anie onboard` must work standalone** — it does not require an existing config or credentials.
2. **`/onboard` must not disrupt an active streaming session** — if the agent is streaming, the slash command should wait or warn.
3. **Terminal setup/teardown must be robust** — the standalone `anie onboard` manages its own terminal; the `/onboard` overlay reuses the existing terminal.
4. **The onboarding screen must be re-entrant** — running `/onboard` multiple times works correctly.
5. **Do not build provider management** — no list/test/delete screen yet (Phase 5).

---

## Test plan

### Required tests

| # | Test | Location |
|---|------|----------|
| 1 | First-run detection (no config, no creds) | `anie-cli` |
| 2 | First-run detection (config exists) | `anie-cli` |
| 3 | First-run detection (creds exist, no config) | `anie-cli` |
| 4 | Subcommand parsing: `anie onboard` | `anie-cli` |
| 5 | Positional prompt still works with subcommand | `anie-cli` |
| 6 | `/onboard` slash command recognized | `anie-tui` |
| 7 | Overlay captures input when active | `anie-tui` |
| 8 | Overlay close restores normal mode | `anie-tui` |
| 9 | Config written after onboarding completion | `anie-cli` |

### Manual validation

1. Delete `~/.anie/` entirely.  Run `anie` — onboarding TUI appears.
2. Complete onboarding — verify `~/.anie/config.toml` and credentials are created.
3. Run `anie` again — normal mode starts (no onboarding).
4. Run `anie onboard` — onboarding TUI appears even though config exists.
5. Inside the running agent, type `/onboard` — overlay appears.
6. Cancel the overlay — normal chat resumes.
7. Complete the overlay — verify config is updated and status bar reflects changes.
8. Verify `anie "hello world"` still works as a one-shot prompt.

---

## Risks

1. **Subcommand + positional arg conflict in clap** — test thoroughly.
2. **Terminal state corruption** — if the standalone onboarding crashes, the terminal must be restored.  Use a panic hook.
3. **Overlay rendering over active content** — the overlay must fully cover the underlying UI to avoid visual artifacts.
4. **Config reload race** — if the controller is mid-request when onboarding completes, config reload must be deferred.

---

## Exit criteria

This phase is complete when:

- [ ] `anie onboard` launches the TUI onboarding screen.
- [ ] First-run auto-detection launches onboarding when appropriate.
- [ ] `/onboard` opens the onboarding overlay inside the running TUI.
- [ ] Config and credentials are written on completion.
- [ ] Post-onboarding config reload works (model/provider updated).
- [ ] Old stdio onboarding code is removed.
- [ ] `anie "prompt"` positional usage still works.
- [ ] All tests pass.

---

## Follow-on phase

After this phase is green, proceed to:
→ `phase_5_provider_management.md`
