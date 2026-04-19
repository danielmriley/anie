# Phase 3 — Onboarding Inline Model Selection

This phase inserts a model-picker step into every onboarding provider-setup path, **inside the existing onboarding card** — not as a separate full-screen layer.

## Why this phase exists

Today's onboarding picks a default model automatically:

- local server → uses the first detected model
- API provider → uses a hardcoded preset (e.g. `claude-sonnet-4-6`, `gpt-4o`)
- custom endpoint → uses whatever the user typed as a model ID

That works but is not **discoverable**. The user never sees what else is available.

After this phase, every provider-setup path will:

1. validate the provider/endpoint
2. discover available models
3. show a picker inside the onboarding card
4. let the user choose

---

## Current onboarding code facts

### State machine (`crates/anie-tui/src/onboarding.rs`)

The relevant states today are:

```rust
enum OnboardingState {
    MainMenu { selected: usize },
    ManagingProviders,
    LocalServerWaiting,
    LocalServerSelect { selected: usize },
    NoLocalServers,
    ProviderPresetList { selected: usize },
    ApiKeyInput { preset_index: usize, input: TextField },
    CustomEndpoint { form: CustomEndpointForm },
    Busy { title, message, return_to },
    Success { message },
    Error { message, return_to },
}
```

### Where models are currently chosen

- **Local server**: `handle_local_select_key()` picks `server.models.first()` on Enter
- **API preset**: `configure_preset_provider()` uses the preset's hardcoded `model`
- **Custom endpoint**: `configure_custom_provider()` uses the user-typed `model_id`

All three paths produce a `ConfiguredProvider` that embeds a chosen `Model`.

### Background worker pattern

Onboarding already uses `mpsc::UnboundedSender<WorkerEvent>` for async operations (local detection, provider validation). The same pattern should be used for model discovery.

---

## Files expected to change

### Modified files

- `crates/anie-tui/src/onboarding.rs` — new states, discovery integration, inline picker rendering

### Used but not modified

- `crates/anie-tui/src/model_picker.rs` — the shared picker from Phase 2 (consumed, not changed)
- `crates/anie-providers-builtin/src/model_discovery.rs` — the discovery service from Phase 1

### Not yet

- `crates/anie-tui/src/app.rs` — `/model` command wiring is Phase 4
- `crates/anie-tui/src/providers.rs` — "View Models" action is Phase 4
- `crates/anie-cli/` — no CLI changes yet

---

## Recommended implementation

### Sub-step A — Add new onboarding states

```rust
OnboardingState::DiscoveringModels {
    /// Which provider path led here.
    context: ModelPickerContext,
    /// Message to display during discovery.
    message: String,
}

OnboardingState::PickingModel {
    /// Which provider path led here.
    context: ModelPickerContext,
    /// The picker itself (owned here, rendered in the card body).
    picker: ModelPickerPane,
}
```

Where `ModelPickerContext` captures what the user already entered so back-navigation can restore it:

```rust
enum ModelPickerContext {
    LocalServer {
        server_index: usize,
        server: LocalServer,
    },
    ApiPreset {
        preset_index: usize,
        preset: ProviderPreset,
        api_key: String,
    },
    CustomEndpoint {
        form_snapshot: CustomEndpointForm,
        api_key: String,
        base_url: String,
        provider_name: String,
    },
}
```

### Sub-step B — Add `WorkerEvent::ModelsDiscovered`

```rust
WorkerEvent::ModelsDiscovered {
    context: ModelPickerContext,
    result: Result<Vec<ModelInfo>, String>,
}
```

### Sub-step C — Wire the local-server path

After the user selects a detected local server (Enter in `LocalServerSelect`):

1. transition to `DiscoveringModels` with a spinner
2. spawn a discovery task: `discover_models(request)` for that server
3. on `ModelsDiscovered` success → transition to `PickingModel` with the models
4. on failure → show error with back/retry option

In `PickingModel`:

- render the picker **inside the onboarding card body** (where the list/form normally renders)
- keep the onboarding title and card framing
- on `ModelPickerAction::Selected(model_info)` → build `ConfiguredProvider` using the selected model
- on `ModelPickerAction::Cancelled` → return to `LocalServerSelect`
- on `ModelPickerAction::Refresh` → re-run discovery

### Sub-step D — Wire the API-preset path

After API key validation succeeds in `configure_preset_provider()`:

1. before building `ConfiguredProvider`, transition to `DiscoveringModels`
2. discover models from the provider's base URL using the validated key
3. on success → `PickingModel`
4. on failure → fall back to using the preset's hardcoded default model (with a warning message)

The fallback is important: if a hosted provider doesn't expose a model-listing endpoint (unlikely but possible), the user shouldn't be stuck.

### Sub-step E — Wire the custom-endpoint path

After endpoint validation succeeds in `configure_custom_provider()`:

1. transition to `DiscoveringModels`
2. discover models from the custom base URL
3. on success → `PickingModel`
4. on failure → allow manual model-ID entry as an explicit escape hatch, clearly labeled

### Sub-step F — Preserve back-navigation state

When the user presses Esc/back from the picker:

- **local server**: return to `LocalServerSelect` with the same server selected
- **API preset**: return to `ApiKeyInput` with the key still in the masked field
- **custom endpoint**: return to `CustomEndpoint` with all form fields preserved

This is why `ModelPickerContext` snapshots the previous state.

### Sub-step G — Render the picker inside the card

The existing onboarding `render()` method draws a centered card. The card body is currently rendered by match arms like `render_local_server_select()`, `render_api_key_input()`, etc.

For the `PickingModel` state:

- render the card frame as usual (title, borders)
- render `picker.render(...)` into the card's inner area
- render footer hints from the picker

The picker's preferred height should be constrained to fit inside the card (e.g. `min(picker.preferred_height(), card_inner.height)`).

### Sub-step H — Update `ConfiguredProvider` construction

Today, `ConfiguredProvider` is built with a full `Model`. After this phase, it should be built from the `ModelInfo` selected in the picker, using `ModelInfo::to_model()` from Phase 1.

For config-backed providers (local/custom), the `Model` needs `base_url` and `api` from the discovery context.

---

## Constraints

1. **Do not open a second full-screen layer.** The picker lives inside the existing card.
2. **Do not lose user-entered state on back-navigation.** API keys and form fields must be preserved.
3. **Do not block if discovery fails.** Provide a fallback path.
4. **Reuse the `ModelPickerPane` from Phase 2.** Do not build a separate picker for onboarding.
5. **Keep the existing card visual style.** Title, borders, and footer remain consistent.

---

## Test plan

### Required unit tests

| # | Test |
|---|------|
| 1 | local-server path transitions to `DiscoveringModels` on selection |
| 2 | `ModelsDiscovered` success transitions to `PickingModel` |
| 3 | `ModelsDiscovered` failure transitions to error state |
| 4 | `ModelPickerAction::Selected` builds `ConfiguredProvider` with selected model |
| 5 | `ModelPickerAction::Cancelled` returns to the previous provider-setup state |
| 6 | API-preset path falls back to hardcoded default on discovery failure |
| 7 | custom-endpoint path preserves form state on back-navigation |
| 8 | `ModelPickerAction::Refresh` re-runs discovery |

### Manual validation

1. `anie onboard` with Ollama running → select server → verify model picker appears inside card
2. pull a new model in another terminal → press `r` in picker → verify new model appears
3. select Anthropic preset → enter API key → verify model picker shows Anthropic models
4. press Esc from picker → verify API key input is still populated
5. custom endpoint → enter local URL → verify picker shows endpoint's models
6. custom endpoint with bad URL → verify error screen, back navigates to form

---

## Exit criteria

- [ ] every provider-setup path transitions into a model picker step
- [ ] the picker renders inside the onboarding card (not as a separate screen)
- [ ] back-navigation preserves user-entered state (keys, form fields, server selection)
- [ ] refresh works (at minimum for local backends)
- [ ] discovery failure has a graceful fallback
- [ ] the resulting `ConfiguredProvider` uses the picker-selected model
- [ ] unit tests cover all three paths (local, preset, custom)

---

## Follow-on phase

→ `phase_4_model_command_and_provider_actions.md`
