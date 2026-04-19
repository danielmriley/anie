# Dynamic Model Menus — Pi-Style Phased Plan

This folder tracks the implementation of **dynamic, context-aware model selection menus** for Anie, inspired by pi-mono's inline `/model` selector pattern.

The previously completed onboarding and keyring credential plans (v0.1.0) have been archived in `completed/`.

---

## Source documents

1. `tmp.md` — original dynamic model selection proposal
2. pi reference implementation (analyzed directly):
   - `interactive-mode.ts` — `showSelector(...)` pattern: selectors **replace the editor area**, keeping transcript and footer visible
   - `components/model-selector.ts` — compact, bordered, search-first model picker with provider badges and current-model marker
   - `components/oauth-selector.ts` — same pattern applied to login flows
   - `docs/tui.md` — pi component system, overlays, and `SelectList` API
3. Phase documents in this folder

---

## Key design decisions from the pi review

### 1. Menus must not be full-screen inside the interactive TUI

pi's pattern is:

- keep the **transcript/chat history visible**
- keep the **footer/status bar visible**
- replace the **editor/input area** with a compact selector component
- restore the editor when the selector closes

Anie must follow the same principle for `/model` and model-browsing actions.

### 2. Search is first-class

pi's model selector puts focus into a **search input** immediately on open. Arrow keys navigate the filtered list. This is critical for usability when a provider advertises many models.

### 3. Exact-match fast path

pi's `/model <query>` does not always open the selector:

1. if `<query>` exactly identifies a known model → switch immediately, no picker
2. otherwise → open picker with `<query>` prefilled in the search field

This gives power users speed while keeping discoverability for everyone.

### 4. Inline feedback, not success screens

pi does not route users through confirmation/success pages after model selection. It:

- closes the selector
- restores the editor
- shows a brief status message (e.g. "Model: claude-sonnet-4-6")

Anie must do the same.

### 5. Onboarding keeps its existing card flow

The model-picker step should appear **inside** the existing onboarding popup/card, not as a separate full-screen layer.

### 6. Provider trait should not be extended directly (yet)

Anie's `ProviderRegistry` is keyed by `ApiKind`, not provider name. One `OpenAIProvider` implementation serves many providers (openai, groq, together, local endpoints, etc.). Model discovery needs per-endpoint context (base URL, auth), so the right first step is a **model discovery service layer**, not a `Provider::list_models()` method.

---

## Phases

| Phase | File | Summary |
|-------|------|---------|
| 1 | `phase_1_model_discovery_and_cache.md` | Backend model-discovery service, `ModelInfo` type, TTL cache |
| 2 | `phase_2_pi_style_selector_host_and_model_picker.md` | Pi-style input-replacement selector host, reusable `ModelPickerPane` |
| 3 | `phase_3_onboarding_inline_model_selection.md` | Onboarding model-picker step inside the existing card flow |
| 4 | `phase_4_model_command_and_provider_actions.md` | `/model` selector, `/providers` "View Models", exact-match fast path |
| 5 | `phase_5_persistence_project_scope_and_cli_listing.md` | Write-target policy, project config support, `anie models` CLI command |
| 6 | `phase_6_polish_docs_and_validation.md` | Visual polish, docs, performance validation, release QA matrix |

---

## Dependency graph

```text
Phase 1 ──► Phase 2 ──► Phase 3
                │            │
                └────────────┴──► Phase 4 ──► Phase 5 ──► Phase 6
```

- **Phase 1** is backend-only (no UI changes).
- **Phase 2** is the core UI piece — the selector host and picker component.
- **Phases 3 and 4** are independent consumers of the shared picker.
- **Phase 5** is persistence rules that make picks durable.
- **Phase 6** is polish and release.

---

## Current codebase facts

### Already present

| Item | Location |
|------|----------|
| Static builtin model catalog | `crates/anie-providers-builtin/src/models.rs` — `builtin_models()` |
| Custom model entries from config | `crates/anie-config/src/lib.rs` — `configured_models()` |
| Local server detection | `crates/anie-providers-builtin/src/local.rs` — `detect_local_servers()`, `probe_openai_compatible()` |
| Model catalog build at startup | `crates/anie-cli/src/controller.rs` — `build_model_catalog()` |
| `ProviderRegistry` keyed by `ApiKind` | `crates/anie-provider/src/registry.rs` |
| Three-pane TUI layout | `crates/anie-tui/src/app.rs` — `layout()` → output pane, status bar, input pane |
| Onboarding screen | `crates/anie-tui/src/onboarding.rs` — `OnboardingScreen` |
| Provider management screen | `crates/anie-tui/src/providers.rs` — `ProviderManagementScreen` |
| Config mutation (comment-preserving) | `crates/anie-config/src/mutation.rs` — `ConfigMutator` |
| `/model <id>` text-based switching | `crates/anie-tui/src/app.rs` — `handle_slash_command()` |
| Overlay system (full-screen) | `crates/anie-tui/src/app.rs` — `OverlayState` enum |
| Controller config reload | `crates/anie-cli/src/controller.rs` — `reload_config()` |
| Project config discovery | `crates/anie-config/src/lib.rs` — `find_project_config()` |

### Not yet present

- dynamic model discovery from remote APIs
- `ModelInfo` normalized type for discovered models
- reusable model picker widget
- pi-style input-pane selector host (as opposed to full-screen overlay)
- search-first model selection flow
- "View Models" provider action
- project-aware model-persistence write-target policy
- CLI model-listing command

---

## Planning conventions

Each phase document includes:

- why the phase exists
- current code facts relevant to that phase
- files expected to change and files that must **not** change yet
- recommended sub-steps in implementation order
- constraints
- test plan (unit/integration/manual)
- exit criteria

---

## Related docs

- `tmp.md` — original proposal
- `docs/onboarding-and-keyring.md` — design document for the completed onboarding/keyring feature
- `docs/arch/onboarding_flow.md` — architecture notes on the current onboarding system
- `docs/arch/credential_resolution.md` — credential resolution order
- `docs/onboarding_plans/completed/README.md` — archived v0.1.0 plan index
