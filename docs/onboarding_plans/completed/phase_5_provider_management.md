# Phase 5 — Provider Management

This phase adds the provider list/test/delete TUI screen and the `/providers` slash command.  It gives users ongoing visibility and control over their configured providers beyond initial onboarding.

## Why this phase exists

After onboarding (Phase 4), users have at least one configured provider.  Over time they will want to:

- See which providers and credentials are set up.
- Test whether a provider's API key is still valid or a local server is still reachable.
- Delete a provider they no longer use.
- Add additional providers without re-running full onboarding.

The design document's "Manage Existing" menu item and the `/providers` slash command cover this need.

---

## Files expected to change

### New files

- `crates/anie-tui/src/providers.rs` — the `ProviderManagementScreen` widget.

### Modified files

- `crates/anie-tui/src/lib.rs` — add `mod providers; pub use providers::ProviderManagementScreen;`.
- `crates/anie-tui/src/app.rs` — add `/providers` slash command; add overlay mode for the management screen.
- `crates/anie-tui/src/onboarding.rs` — wire "Manage Existing Providers" main menu item to the management screen (or embed it inline).
- `crates/anie-cli/src/controller.rs` — handle `UiAction::ManageProviders` and post-management config reload.
- `crates/anie-auth/src/store.rs` — possibly add `get_all()` or similar if `list_providers()` is insufficient.

---

## Sub-steps

### Sub-step A — Define `ProviderManagementScreen`

```rust
pub struct ProviderManagementScreen {
    providers: Vec<ProviderEntry>,
    selected: usize,
    action_menu: Option<ActionMenu>,
    credential_store: CredentialStore,
    test_results: HashMap<String, TestResult>,
}

pub struct ProviderEntry {
    pub name: String,
    pub provider_type: ProviderType, // Local, ApiKey, Custom
    pub base_url: Option<String>,
    pub default_model: String,
    pub has_credential: bool,
}

pub enum ProviderType {
    Local,
    ApiKey,
    Custom,
}

pub enum TestResult {
    Pending,
    Success { latency_ms: u64 },
    Failed { error: String },
}

pub enum ActionMenu {
    /// Showing actions for the selected provider.
    Open { items: Vec<ActionItem>, selected: usize },
}

pub enum ActionItem {
    TestConnection,
    DeleteProvider,
    EditApiKey,
    SetAsDefault,
}

pub enum ProviderManagementAction {
    Continue,
    Close,
    ConfigChanged, // provider deleted or default changed
}
```

### Sub-step B — Build the provider table view

Render a table showing all configured providers:

```
╔══════════════════════════════════════════════════════════════╗
║                    Configured Providers                      ║
╟──────────────────────────────────────────────────────────────╢
║  Provider     │ Type   │ Model        │ Key  │ Status       ║
║  ─────────────┼────────┼──────────────┼──────┼────────────  ║
║ › anthropic   │ API    │ claude-s-4-6 │  ●   │ ✓ OK (142ms) ║
║   openai      │ API    │ gpt-4o       │  ●   │ ─ untested   ║
║   ollama      │ Local  │ qwen3:32b    │  ─   │ ✓ OK (8ms)   ║
║                                                              ║
║  [↑↓] Navigate  [Enter] Actions  [t] Test  [d] Delete  [q] Close ║
╚══════════════════════════════════════════════════════════════╝
```

Use `ratatui::widgets::Table` with styled rows:
- Green row for tested-OK providers.
- Red row for failed providers.
- Default for untested.
- The `Key` column shows `●` if a credential exists, `─` if not.

Data sources:
- Provider names and config from `AnieConfig.providers` + `builtin_models()`.
- Credential existence from `CredentialStore::list_providers()`.
- Test results from the `test_results` map.

### Sub-step C — Implement connection testing

Add a `test_provider()` async function:

```rust
pub async fn test_provider(entry: &ProviderEntry, credential_store: &CredentialStore) -> TestResult;
```

For **local providers** (Ollama, LM Studio):
- HTTP GET to `{base_url}/v1/models`.
- Success if 200 response.
- Report latency.

For **API key providers** (Anthropic, OpenAI, etc.):
- Use a minimal API call appropriate to the provider:
  - OpenAI: `GET /v1/models` with `Authorization: Bearer {key}`.
  - Anthropic: `POST /v1/messages` with a tiny payload and `x-api-key` header (or `GET /v1/models` if available).
- Success if authenticated response.
- Report latency.

For **custom providers**:
- Same as local — `GET /v1/models`.

**Async pattern:** Same as Phase 3 — spawn the test task, poll via `handle_tick()`, update `test_results` map.

Keyboard shortcut: `t` on a selected provider triggers a test.  `T` (shift) tests all providers.

### Sub-step D — Implement provider deletion

When the user selects "Delete" from the action menu or presses `d`:

1. Show a confirmation prompt: "Delete provider 'anthropic'? This removes the API key. [y/N]"
2. On confirmation:
   - `CredentialStore::delete(provider_name)`.
   - Remove from `AnieConfig.providers`.
   - Write updated config.
   - Remove from the table.
   - Return `ProviderManagementAction::ConfigChanged`.

### Sub-step E — Implement "Edit API Key"

When the user selects "Edit API Key":

1. Show a masked input field (same component as onboarding key input).
2. On submit:
   - `CredentialStore::set(provider_name, &new_key)`.
   - Mark provider as untested (clear test result).
3. On cancel (`Esc`): return to table.

### Sub-step F — Implement "Set as Default"

When the user selects "Set as Default":

1. Update `AnieConfig.model.provider` and `AnieConfig.model.id` to the selected provider's default model.
2. Write updated config.
3. Return `ProviderManagementAction::ConfigChanged`.
4. Show a brief success message: "✓ Default provider set to anthropic (claude-sonnet-4-6)".

### Sub-step G — Wire `/providers` slash command

In `crates/anie-tui/src/app.rs`:

1. Add `UiAction::ManageProviders`.
2. Parse `/providers` → emit `UiAction::ManageProviders`.
3. Open `ProviderManagementScreen` as a modal overlay (same pattern as `/onboard`).
4. On `ProviderManagementAction::ConfigChanged` → notify controller to reload config.
5. On `ProviderManagementAction::Close` → close overlay.

### Sub-step H — Integrate into onboarding main menu

In the `OnboardingScreen` main menu (Phase 3), the "Manage Existing Providers" item should transition to the `ProviderManagementScreen` if providers already exist.  If no providers exist yet, this item can be grayed out or hidden.

### Sub-step I — Tests

1. **Table data population** — given a config with 3 providers, verify 3 rows render.
2. **Navigation** — `↑`/`↓` moves selection.
3. **Test action** — pressing `t` transitions selected provider to `Pending` state.
4. **Delete confirmation** — `d` shows confirmation, `y` removes, `n` cancels.
5. **Edit key** — opens masked input, submitting stores via credential store.
6. **Set as default** — updates config and returns `ConfigChanged`.
7. **Close** — `q` or `Esc` returns `Close`.

---

## Constraints

1. **Provider management is read-write** — it can modify credentials and config.  All writes must be atomic and safe.
2. **The management screen must reflect current state** — if credentials were added via `anie onboard`, they should appear immediately.
3. **Do not auto-test on open** — testing is user-initiated to avoid unnecessary API calls and latency on screen open.
4. **Deletion is permanent** — keyring entry and config entry are both removed.  The confirmation prompt is mandatory.
5. **The screen must work both as a standalone overlay and embedded in onboarding.**

---

## Test plan

### Required tests

| # | Test | Location |
|---|------|----------|
| 1 | Table shows all configured providers | `anie-tui` |
| 2 | Navigation works | `anie-tui` |
| 3 | Test action transitions to Pending | `anie-tui` |
| 4 | Delete with confirmation removes provider | `anie-tui` |
| 5 | Delete cancellation preserves provider | `anie-tui` |
| 6 | Edit key stores via CredentialStore | `anie-tui` |
| 7 | Set as default updates config | `anie-tui` |
| 8 | `/providers` slash command recognized | `anie-tui` |
| 9 | Overlay opens and closes cleanly | `anie-tui` |

### Manual validation

1. Configure 2–3 providers via onboarding.
2. Type `/providers` — management screen opens.
3. Press `t` on each — verify test results appear.
4. Edit an API key — verify the new key is stored.
5. Delete a provider — verify it disappears from the table and from the config file.
6. Set a different default — verify status bar updates.
7. Close the screen — verify normal chat resumes.

---

## Risks

1. **Config file write conflicts** — if the controller is also writing config (unlikely), use file locking or serialize writes.
2. **Stale test results** — if a provider is edited, clear its test result.
3. **Keyring enumeration limitations** — some keyring backends don't support listing entries.  Rely on config file + credential store cross-reference.

---

## Exit criteria

This phase is complete when:

- [ ] `/providers` opens the provider management screen.
- [ ] Provider table displays all configured providers with credential status.
- [ ] Connection testing works for local and API providers.
- [ ] Provider deletion works with confirmation.
- [ ] API key editing works with masked input.
- [ ] "Set as default" updates the active provider.
- [ ] The management screen is accessible from the onboarding main menu.
- [ ] All tests pass.

---

## Follow-on phase

After this phase is green, proceed to:
→ `phase_6_polish_and_docs.md`
