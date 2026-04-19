# Completed Onboarding & Keyring Plans

These are the completed v0.1.0 onboarding/keyring implementation plans that have already been shipped.

---

# Onboarding & Keyring — Phased Implementation Plan

This folder breaks the design in `docs/onboarding-and-keyring.md` into ordered implementation phases.

## Source documents

1. `docs/onboarding-and-keyring.md` — original design document
2. The phase documents in this folder

If this folder and the design document drift apart, update the phase docs first and
then reconcile the design document.

---

## Phases in this folder

- `phase_1_credential_store.md` — Add `keyring` dependency and build the `CredentialStore` abstraction in `anie-auth`.
- `phase_2_migration_and_resolver.md` — Migrate existing `auth.json` credentials into the keyring and update `AuthResolver` to use `CredentialStore`.
- `phase_3_onboarding_tui.md` — Build the `OnboardingScreen` ratatui widget in `anie-tui` with menu-driven provider setup.
- `phase_4a_safe_config_mutation.md` — Add `toml_edit` to `anie-config` for non-destructive config writes.
- `phase_4_cli_wiring.md` — Wire the `anie onboard` subcommand and `/onboard` slash command, and update first-run detection.
- `phase_5_provider_management.md` — Add the provider list/test/delete TUI screen and the `/providers` slash command.
- `phase_6_polish_and_docs.md` — Visual polish, documentation updates, README refresh, and release validation.

---

## Planning conventions

Each phase document includes:

- Why the phase exists
- What code should change
- What should **not** change yet
- Sub-steps in recommended implementation order
- Required tests and manual validation
- Exit criteria / gate conditions

These follow the same conventions established in `docs/completed/phased_plan_v1-0-1/`.

---

## Current state summary

### Already present

- `anie-auth` crate with `AuthStore`, `AuthResolver`, `save_api_key()`, and `load_auth_store()` — all JSON-file-backed.
- `anie-cli/src/onboarding.rs` with a basic stdio-driven first-run flow (provider selection, API key prompt, local server detection).
- `anie-config` with `AnieConfig`, `ProviderConfig`, global/project config loading.
- `anie-providers-builtin` with `detect_local_servers()`, `builtin_models()`, and the hosted model catalog.
- `anie-tui` with `App`, `OutputPane`, `InputPane`, and the existing event loop.

### Not yet present

- `keyring` dependency
- `CredentialStore` abstraction
- JSON → keyring migration logic
- TUI-based onboarding screen (`OnboardingScreen`)
- `anie onboard` subcommand
- `/onboard` slash command
- Provider management screen (list/test/delete)

---

## Dependency graph

```
Phase 1  ──►  Phase 2  ──►  Phase 3  ──► Phase 4a ──► Phase 4
                                │                      │
                                └──────────────────────┴──►  Phase 5  ──►  Phase 6
```

Phases 1 and 2 are backend-only.  Phase 3 builds the TUI widget.  Phase 4 wires everything together.  Phases 5 and 6 add management features and polish.

---

## Related docs

- `docs/onboarding-and-keyring.md`
- `docs/completed/phased_plan_v1-0-1/README.md` (conventions reference)
