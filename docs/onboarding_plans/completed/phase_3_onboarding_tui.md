# Phase 3 — Onboarding TUI Screen

This phase builds the menu-driven `OnboardingScreen` widget inside `anie-tui` using `ratatui`.  It replaces the current stdio-based first-run flow with a polished, keyboard-driven full-screen experience.

## Why this phase exists

The current onboarding in `crates/anie-cli/src/onboarding.rs` uses raw `println!` / `stdin().read_line()` prompts.  The design document calls for a TUI-first experience inspired by pi-mono's `/login` and `/settings` flows — navigable menus, masked key input, connection testing, and visual feedback.

This phase creates the widget and its internal state machine.  Phase 4 wires it into the CLI and the agent TUI as a slash command.

---

## Files expected to change

### New files

- `crates/anie-tui/src/onboarding.rs` — the `OnboardingScreen` widget and its state machine.

### Modified files

- `crates/anie-tui/src/lib.rs` — add `mod onboarding; pub use onboarding::OnboardingScreen;`.
- `crates/anie-tui/Cargo.toml` — add dependencies if needed (e.g., `anie-auth`, `anie-providers-builtin` for local detection and credential storage).

### No changes yet

- `crates/anie-cli/src/onboarding.rs` — will be replaced in Phase 4.
- `crates/anie-cli/src/lib.rs` — CLI wiring happens in Phase 4.

---

## Sub-steps

### Sub-step A — Define the screen state machine

The onboarding screen has the following states:

```
MainMenu
  ├── LocalServerSetup
  │     ├── Detecting (spinner)
  │     ├── ServerFound (confirm prompt)
  │     └── ServerConfigured (success)
  ├── AddApiKeyProvider
  │     ├── ProviderList (preset selection)
  │     ├── KeyInput (masked text field)
  │     ├── TestingConnection (spinner)
  │     └── ProviderConfigured (success)
  ├── CustomEndpoint
  │     ├── EndpointForm (base_url, provider_name, model_id, api_key)
  │     ├── TestingConnection (spinner)
  │     └── EndpointConfigured (success)
  └── Done
```

Define an `OnboardingState` enum capturing these states.  Each state variant carries the data it needs (e.g., detected servers, current form field values, error messages).

### Sub-step B — Define the `OnboardingScreen` struct

```rust
pub struct OnboardingScreen {
    state: OnboardingState,
    credential_store: CredentialStore,
    /// Completed provider configurations ready to be written to config.
    configured_providers: Vec<ConfiguredProvider>,
}

/// A provider successfully configured during onboarding.
pub struct ConfiguredProvider {
    pub provider_name: String,
    pub base_url: Option<String>,
    pub default_model: String,
    pub is_local: bool,
}

/// Actions the onboarding screen needs the caller to perform.
pub enum OnboardingAction {
    /// Onboarding is complete — return the configured providers.
    Complete(Vec<ConfiguredProvider>),
    /// User cancelled onboarding.
    Cancelled,
    /// No action yet — continue rendering.
    Continue,
}
```

The screen does **not** own the terminal or event loop.  It exposes:

```rust
impl OnboardingScreen {
    pub fn new(credential_store: CredentialStore) -> Self;
    pub fn handle_key(&mut self, key: KeyEvent) -> OnboardingAction;
    pub fn handle_tick(&mut self) -> OnboardingAction;
    pub fn render(&self, frame: &mut Frame, area: Rect);
}
```

This follows the same pattern as the existing `App` — external code drives the event loop and calls into the screen.

### Sub-step C — Implement the Main Menu

Render a bordered box with the welcome header and numbered menu items:

```
╔══════════════════════════════════════════════════════════════╗
║                  Welcome to Anie — First Run                 ║
╟──────────────────────────────────────────────────────────────╢
║  › Configure Local Server                                    ║
║    Add API Key Provider                                      ║
║    Custom OpenAI-compatible Endpoint                         ║
║                                                              ║
║  [↑↓] Navigate   [Enter] Select   [q] Quit                  ║
╚══════════════════════════════════════════════════════════════╝
```

- Use `ratatui::widgets::List` with `ListState` for cursor tracking.
- `↑`/`↓` or `j`/`k` to navigate.
- `Enter` to select.
- `q` or `Esc` to cancel/quit.
- The first item should show a detection hint if a local server was auto-detected (e.g., "Configure Local Server (Ollama detected ✓)").

### Sub-step D — Implement Local Server Setup sub-flow

1. **Detecting** — show a spinner + "Scanning for local servers…".
   - Call `detect_local_servers()` (needs to be async — use `handle_tick()` to poll a spawned task or run detection before entering this state).
   - Design decision: run detection **before** entering this sub-flow (during `MainMenu` init or on first `handle_tick`) so the results are available immediately.

2. **ServerFound** — show detected server name, URL, and model list.
   - "Use Ollama on http://localhost:11434? [Y/n]"
   - If multiple servers found, present a selection list.
   - If no servers found, show "No local servers detected" with an option to enter a custom URL.

3. **ServerConfigured** — green success banner.
   - "✅ Configured ollama with model qwen3:32b as default provider."
   - Save credential (if any) via `CredentialStore`.
   - Add to `configured_providers`.
   - Auto-return to `MainMenu` after a short delay or on any keypress, or advance to `Done`.

### Sub-step E — Implement Add API Key Provider sub-flow

1. **ProviderList** — a selectable list of presets:
   ```
   • Anthropic (Claude)
   • OpenAI (GPT-4o, o1, etc.)
   • xAI / Grok
   • Groq
   • Together.ai
   • Fireworks
   • Mistral
   ```
   Each preset carries: `provider_name`, `base_url`, `default_model_id`, `env_var_name`.

2. **KeyInput** — masked text input field.
   - Show the provider name and a prompt: "Enter your Anthropic API key:"
   - Characters display as `•` (masked).
   - `Enter` to submit, `Esc` to go back.
   - Build this using the existing `InputPane` text-editing logic or a simpler single-line masked input.

3. **TestingConnection** — spinner + "Verifying API key…"
   - Make a lightweight API call (e.g., list models or a minimal completion) to validate the key.
   - This requires async — same pattern as local server detection.
   - On success → `ProviderConfigured`.
   - On failure → show red error banner with the error message + option to retry or go back.

4. **ProviderConfigured** — green success banner.
   - Save key via `CredentialStore::set()`.
   - Add to `configured_providers`.

### Sub-step F — Implement Custom Endpoint sub-flow

1. **EndpointForm** — a multi-field form:
   - Base URL (text input, not masked)
   - Provider name (text input, default: "custom")
   - Default model ID (text input)
   - API key (masked input, optional — "leave empty for local providers")
   - `Tab` / `Shift+Tab` to move between fields.
   - `Enter` on the last field (or a "Submit" button) to proceed.

2. **TestingConnection** / **EndpointConfigured** — same pattern as API key flow.

### Sub-step G — Implement the Done state

When the user finishes configuring at least one provider (or selects "Done" from the main menu after configuration):

- Show a summary of what was configured.
- "✅ All set! Starting anie…" or "Press Enter to start".
- Return `OnboardingAction::Complete(configured_providers)`.

### Sub-step H — Visual polish

- Use the existing `anie-tui` color conventions (check `app.rs` for style constants).
- Green (`Color::Green`) for success banners.
- Red (`Color::Red`) for error messages.
- Cyan or default for informational text.
- Spinner animation using `handle_tick()` with a frame counter (e.g., `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`).
- Footer bar with context-sensitive keybinding hints (like pi-mono).

### Sub-step I — Unit tests

Test the state machine logic without rendering:

1. **Main menu navigation** — `↓` moves selection, `Enter` transitions to correct sub-state.
2. **Provider list selection** — selecting "Anthropic" transitions to key input with correct provider metadata.
3. **Key input handles editing** — typing characters, backspace, masked display.
4. **Escape returns to previous state** — from any sub-flow back to main menu.
5. **Quit from main menu** — `q` returns `OnboardingAction::Cancelled`.
6. **Completion** — after configuring a provider, `Done` returns `OnboardingAction::Complete(...)`.

---

## Async design considerations

The onboarding screen needs async operations (local server detection, API key validation).  Two approaches:

**Option A — Pre-fetch + polling:**
- Spawn detection tasks before rendering.
- `handle_tick()` polls for results via `tokio::sync::oneshot`.
- Simpler to reason about, keeps the widget sync.

**Option B — Async widget methods:**
- Make `handle_key()` async.
- More natural but complicates the event loop.

**Recommendation:** Option A.  It matches the existing `App` pattern where the event loop is sync and async work is dispatched through channels.

---

## Constraints

1. **The `OnboardingScreen` does not own the terminal** — it renders into a provided `Frame` + `Rect`.
2. **All state transitions are driven by keyboard events** — no mouse interaction required (can be added later).
3. **The screen must be re-entrant** — calling it again after initial setup should work (for `/onboard` re-runs).
4. **Do not wire into CLI or slash commands yet** — Phase 4 handles that.
5. **Do not build the provider management (list/test/delete) screen** — Phase 5 handles that.

---

## Test plan

### Required unit tests

| # | Test | Description |
|---|------|-------------|
| 1 | Menu navigation | `↑`/`↓` move selection, wraps around |
| 2 | Menu selection | `Enter` transitions to correct sub-state |
| 3 | Provider preset data | Each preset has valid provider_name, base_url, default_model |
| 4 | Masked input | Characters stored but rendered as `•` |
| 5 | Escape navigation | `Esc` returns to parent state from any sub-flow |
| 6 | Quit action | `q` on main menu returns `Cancelled` |
| 7 | Completion action | After configuration, returns `Complete` with providers |
| 8 | Form field navigation | `Tab`/`Shift+Tab` cycles custom endpoint form fields |

### Render tests (snapshot-style)

| # | Test | Description |
|---|------|-------------|
| 1 | Main menu render | Verify menu items appear in correct order |
| 2 | Success banner render | Green text + checkmark |
| 3 | Error banner render | Red text + error message |

### Manual validation

1. Instantiate `OnboardingScreen` in a test harness with a real terminal.
2. Navigate all menu paths.
3. Verify visual appearance matches design mockups.
4. Test with very small terminal sizes (80×24 minimum).
5. Test with very large terminal sizes.

---

## Risks

1. **Async coordination complexity** — keep it simple with pre-fetch + oneshot channels.
2. **Input handling conflicts** — the onboarding screen captures all keys when active; ensure nothing leaks to the underlying `App`.
3. **Terminal size constraints** — the form layout must degrade gracefully on small terminals.
4. **API key validation endpoints differ per provider** — start with a simple "does the HTTP endpoint respond?" check; full validation can be enhanced later.

---

## Exit criteria

This phase is complete when:

- [ ] `OnboardingScreen` renders the main menu with all three entry paths.
- [ ] Local server detection flow works (detect → confirm → save).
- [ ] API key provider flow works (select → enter key → test → save).
- [ ] Custom endpoint flow works (form → test → save).
- [ ] Credentials are stored via `CredentialStore`.
- [ ] State machine unit tests pass.
- [ ] The widget compiles and renders in isolation (test harness).
- [ ] The existing `App` and TUI are not modified.

---

## Follow-on phase

After this phase is green, proceed to:
→ `phase_4a_safe_config_mutation.md`
