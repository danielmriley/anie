# Phase 6 — Polish, Documentation, and Release Validation

This phase covers visual refinements, documentation updates, architecture docs, and end-to-end validation before the onboarding feature ships.

## Why this phase exists

Phases 1–5 build the functional implementation.  This phase ensures the feature is polished, documented, and tested as a cohesive whole before release.  It also captures architecture decisions for future maintainers.

---

## Files expected to change

### Documentation

- `README.md` — update "Getting Started" section with new first-run experience; add `anie onboard` to command reference.
- `docs/arch/credential_resolution.md` — new file documenting how providers resolve (keyring → JSON fallback → env).
- `docs/arch/onboarding_flow.md` — new file documenting the onboarding state machine and entry points.
- `docs/onboarding-and-keyring.md` — update status from "Proposed" to "Implemented"; add notes on any design deviations.

### Visual polish

- `crates/anie-tui/src/onboarding.rs` — final visual tweaks.
- `crates/anie-tui/src/providers.rs` — final visual tweaks.
- `crates/anie-tui/src/app.rs` — status bar updates for onboarding state.

### Cleanup

- `crates/anie-auth/src/lib.rs` — evaluate removing `#[deprecated]` functions if no external consumers remain.
- `crates/anie-cli/src/onboarding.rs` — remove any dead code from the old stdio flow.

---

## Sub-steps

### Sub-step A — Visual consistency audit

Review all onboarding and provider management screens for visual consistency:

1. **Color palette** — verify all screens use the same color conventions as the main TUI:
   - Green for success.
   - Red for errors.
   - Cyan/blue for informational highlights.
   - Default terminal colors for body text.

2. **Border style** — consistent use of `Block::bordered()` or equivalent.

3. **Footer keybinding hints** — every screen shows context-sensitive keybinding help in the footer, using the same style.

4. **Spinner style** — the same spinner character set (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`) is used everywhere.

5. **Minimum terminal size** — test with 80×24.  Ensure:
   - Text does not overflow.
   - Menu items are not truncated.
   - Forms remain usable.

6. **Large terminal size** — content should be centered or constrained to a readable width (max ~100 columns for the onboarding box).

### Sub-step B — Error message quality

Audit all error paths in onboarding and provider management:

1. **Keyring failures** — "Could not access OS keyring. Credentials saved to ~/.anie/auth.json instead."
2. **Connection test failures** — show the actual error (timeout, connection refused, 401 unauthorized) rather than generic "failed".
3. **Config write failures** — "Could not write ~/.anie/config.toml: permission denied."
4. **Invalid API key format** — if detectable, warn before testing (e.g., OpenAI keys start with `sk-`).

Ensure all error messages are:
- User-facing (no raw Rust error types).
- Actionable (tell the user what to do).
- Visible (red banner, not just a log line).

### Sub-step C — Write architecture documentation

Create `docs/arch/credential_resolution.md`:

```markdown
# Credential Resolution Order

When resolving an API key for a request, the following sources are checked in order:

1. `--api-key` CLI flag (highest priority, single-session override)
2. OS keyring via `CredentialStore` (macOS Keychain, Windows Credential Manager, Linux Secret Service)
3. JSON fallback file `~/.anie/auth.json` (for headless environments or keyring failures)
4. Environment variable (configured via `api_key_env` in provider config, or builtin defaults like `OPENAI_API_KEY`)

## Platform details
...

## Migration
...
```

Create `docs/arch/onboarding_flow.md`:

```markdown
# Onboarding Flow

## Entry points
1. Automatic first-run detection
2. `anie onboard` subcommand
3. `/onboard` slash command

## State machine
...

## Config generation
...
```

### Sub-step D — Update README

Update the main `README.md`:

1. **Getting Started** section:
   - "Run `anie` to start. On first run, an interactive setup wizard will guide you through provider configuration."
   - Screenshot or ASCII art of the onboarding screen.

2. **Commands** section:
   - Add `anie onboard` with description.

3. **Slash Commands** section:
   - Add `/onboard` and `/providers`.

4. **Configuration** section:
   - Document credential storage: "API keys are stored in your operating system's encrypted credential store. On systems without keyring support, keys are stored in `~/.anie/auth.json` with restricted permissions."

### Sub-step E — Update design document status

In `docs/onboarding-and-keyring.md`:

- Change status from "Proposed" to "Implemented in v0.X".
- Add a "Deviations" section noting any differences between the original design and the actual implementation.
- Update the implementation checklist with completion dates.

### Sub-step F — Cleanup deprecated code

Evaluate whether the deprecated functions in `anie-auth` can be removed:

- `save_api_key()` / `save_api_key_to()`
- `load_auth_store()` / `load_auth_store_from()`
- `auth_file_path()`

Search the codebase for any remaining callers:

```bash
rg "save_api_key\b|load_auth_store\b|auth_file_path\b" --type rust
```

If no callers remain outside of tests, remove the deprecated functions and update tests.  If external consumers exist (e.g., integration tests), keep them deprecated for one more release.

### Sub-step G — End-to-end validation

Run through the complete onboarding experience on each supported platform:

#### Scenario 1 — Clean first run (no prior config)
1. Delete `~/.anie/` entirely.
2. Run `anie`.
3. Verify onboarding TUI appears.
4. Configure a local server (if available) or API key provider.
5. Verify `~/.anie/config.toml` is created.
6. Verify credentials are in OS keyring.
7. Verify normal chat starts after onboarding.

#### Scenario 2 — Migration from existing JSON auth
1. Create `~/.anie/auth.json` with a test API key.
2. Delete `~/.anie/config.toml`.
3. Run `anie`.
4. Verify migration message appears in logs.
5. Verify `auth.json.migrated` exists.
6. Verify credential is accessible via keyring.

#### Scenario 3 — `anie onboard` re-run
1. With existing config, run `anie onboard`.
2. Add a second provider.
3. Verify config is updated (not overwritten).
4. Verify both providers work.

#### Scenario 4 — `/onboard` inside running TUI
1. Start `anie` normally.
2. Type `/onboard`.
3. Complete onboarding flow.
4. Verify status bar updates.
5. Verify the new provider is usable immediately.

#### Scenario 5 — `/providers` management
1. Type `/providers`.
2. Test all configured providers.
3. Delete one provider.
4. Verify it's removed from config and keyring.
5. Close management screen.
6. Verify normal chat resumes.

#### Scenario 6 — Headless / CI environment
1. Build with `--no-default-features` for `anie-auth` (no keyring).
2. Set API key via environment variable.
3. Run `anie --print "hello"`.
4. Verify it works without keyring.

#### Scenario 7 — Edge cases
1. Run onboarding and cancel immediately (`q`).
2. Enter an invalid API key — verify error message.
3. Enter a valid key, then immediately `/providers` → delete it.
4. Resize terminal during onboarding — verify layout adapts.
5. Ctrl+C during onboarding — verify terminal is restored.

### Sub-step H — Performance check

Verify that onboarding does not add perceptible latency to normal startup:

1. Time `anie --print "hello"` with existing config.
2. Ensure `check_first_run()` adds < 5ms (config file existence check + credential store check).
3. Ensure keyring access on `AuthResolver::resolve()` adds < 10ms per call on average.

---

## Constraints

1. **No new features in this phase** — only polish, docs, and validation.
2. **Architecture docs should be concise** — one page each, not exhaustive.
3. **README changes should be minimal** — update existing sections, don't restructure.

---

## Test plan

### Required

| # | Test | Type |
|---|------|------|
| 1 | All existing unit tests pass | `cargo test` |
| 2 | All existing integration tests pass | `cargo test -p anie-integration-tests` |
| 3 | End-to-end scenarios 1–7 | Manual |
| 4 | Startup latency check | Manual timing |

### Optional

| # | Test | Type |
|---|------|------|
| 1 | Cross-platform keyring test (macOS) | Manual on macOS |
| 2 | Cross-platform keyring test (Windows) | Manual on Windows |
| 3 | Screenshot comparison for visual consistency | Manual |

---

## Exit criteria

This phase is complete — and the feature is ready to ship — when:

- [ ] All screens are visually consistent with the main TUI.
- [ ] Error messages are user-friendly and actionable.
- [ ] `docs/arch/credential_resolution.md` exists and is accurate.
- [ ] `docs/arch/onboarding_flow.md` exists and is accurate.
- [ ] `README.md` documents `anie onboard`, `/onboard`, and `/providers`.
- [ ] `docs/onboarding-and-keyring.md` status is updated.
- [ ] Deprecated code is cleaned up or tagged for next-release removal.
- [ ] All 7 end-to-end scenarios pass.
- [ ] Startup latency is unaffected for existing users.
- [ ] All automated tests pass.

---

## Post-ship future work

These are explicitly **out of scope** for this phase but noted for future planning:

- **OAuth support** (Azure, Google Vertex) — new credential type in `CredentialStore`.
- **"Save as default model"** after a successful provider test.
- **Telemetry opt-in** during onboarding.
- **Provider health monitoring** — periodic background connection checks.
- **Model picker integration** — `/model` command that shows models from all configured providers.
