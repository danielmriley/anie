# Phase 4 — `/model` Command and Provider Model Actions

This phase wires the shared `ModelPickerPane` into the interactive TUI commands and the provider-management screen, completing the pi-style model-selection experience.

## Why this phase exists

After Phases 1–3:

- model discovery works (Phase 1)
- a reusable picker component exists and can replace the input pane (Phase 2)
- onboarding uses the picker (Phase 3)

But the **most common** model-switching path — the `/model` command during a chat session — still just sends a text string. This phase upgrades it to the full pi-style flow.

---

## Pi reference behavior (recap)

In pi's `interactive-mode.ts`:

- `/model` → opens the model selector in the editor slot
- `/model foo`:
  1. try exact match against known models
  2. if found → switch immediately, show status message
  3. if not found → open selector with `foo` prefilled in search
- `Ctrl+L` → opens model selector (keyboard shortcut)
- closing the selector → restores editor, shows brief "Model: x" status

---

## Current Anie code facts

### `/model` handling (`crates/anie-tui/src/app.rs`)

```rust
"/model" => match arg {
    None => /* print current model */,
    Some(model_id) => {
        let _ = self.action_tx.try_send(UiAction::SetModel(model_id.to_string()));
        self.output_pane.add_system_message(format!("Requested model switch: {model_id}"));
    }
}
```

### `UiAction::SetModel` controller handling (`crates/anie-cli/src/controller.rs`)

```rust
UiAction::SetModel(requested) => {
    self.state.set_model(&requested).await?;
    let _ = self.event_tx.send(self.state.status_event()).await;
}
```

Where `set_model()` calls `resolve_requested_model()` which does substring matching against the current model catalog.

### Provider management actions (`crates/anie-tui/src/providers.rs`)

Current `ActionItem` enum:

```rust
enum ActionItem {
    TestConnection,
    EditApiKey,
    SetAsDefault,
    DeleteProvider,
}
```

No "View Models" action today.

---

## Files expected to change

### Modified files

- `crates/anie-tui/src/app.rs` — `/model` command rewrite, keyboard shortcut, model picker lifecycle
- `crates/anie-tui/src/providers.rs` — add `ViewModels` action item
- `crates/anie-tui/src/tests.rs` — new command behavior tests
- `crates/anie-cli/src/controller.rs` — possibly add `UiAction::RequestModelDiscovery`

### Used but not modified

- `crates/anie-tui/src/model_picker.rs` — consumed by both app and providers

---

## Recommended implementation

### Sub-step A — Redefine `/model` command behavior

Replace the current `/model` handler with:

```rust
"/model" => match arg {
    None => {
        // Open the picker for the current provider
        self.open_model_picker_for_current_provider(None);
    }
    Some(query) => {
        // Try exact match first
        if self.try_exact_model_switch(query) {
            // Switched immediately — done
        } else {
            // No exact match — open picker with query prefilled
            self.open_model_picker_for_current_provider(Some(query.to_string()));
        }
    }
}
```

Where:

```rust
fn try_exact_model_switch(&mut self, query: &str) -> bool {
    // Check current model catalog for exact provider:id or bare id match
    // If found, send UiAction::SetModel and show status message
    // Return true if match found
}
```

This mirrors pi's `handleModelCommand(...)` + `findExactModelMatch(...)`.

### Sub-step B — Add `Ctrl+O` keyboard shortcut for model picker

Currently `Ctrl+O` sends `UiAction::SelectModel`. Repurpose it to open the picker directly:

```rust
(KeyModifiers::CONTROL, KeyCode::Char('o')) => {
    self.open_model_picker_for_current_provider(None);
}
```

### Sub-step C — Implement `open_model_picker_for_current_provider()`

```rust
fn open_model_picker_for_current_provider(&mut self, initial_search: Option<String>) {
    if self.agent_state != AgentUiState::Idle {
        self.output_pane.add_system_message(
            "Cannot open model picker while a run is active.".to_string()
        );
        return;
    }

    // Use the existing model catalog as a starting point
    // Spawn a background discovery refresh if appropriate
    // Convert catalog models to ModelInfo
    // Open the picker via bottom_pane transition
    self.bottom_pane = BottomPane::ModelPicker(
        ModelPickerPane::new(models, current_provider, current_model_id, initial_search)
    );
}
```

### Sub-step D — Handle picker actions in the App event loop

When the bottom pane is `ModelPicker`:

- `ModelPickerAction::Selected(model_info)`:
  1. close picker (restore editor)
  2. send `UiAction::SetModel(model_info.id)` or a richer action carrying the full `ModelInfo`
  3. show system message: `"Model: {model_info.id}"`

- `ModelPickerAction::Cancelled`:
  1. close picker
  2. no message needed

- `ModelPickerAction::Refresh`:
  1. set picker to loading state
  2. spawn discovery task
  3. on completion, call `picker.set_models(...)` or `picker.set_error(...)`

### Sub-step E — Decide async discovery integration for `/model`

Two options:

**Option A — use cached models only (simpler)**:
- `/model` opens picker with whatever models are in the current catalog + cache
- background refresh can update later
- Refresh key does an explicit re-fetch

**Option B — always discover on open (fresher)**:
- `/model` shows loading state, runs discovery, then shows picker
- slower first open but always fresh

**Recommended**: Option A for v1. Use the existing catalog + cache. The `r` key does an explicit refresh if the user wants fresh data.

### Sub-step F — Add "View Models" action to provider management

In `crates/anie-tui/src/providers.rs`, add:

```rust
enum ActionItem {
    TestConnection,
    ViewModels,     // ← new
    EditApiKey,
    SetAsDefault,
    DeleteProvider,
}
```

The "View Models" action should:

1. transition to a loading/busy state
2. run model discovery for that provider
3. display the picker inside the provider-management panel body
4. on selection → update the provider's default model in config
5. on cancel → return to the action menu

Since provider management currently renders as a full-screen popup, the picker should render **inside that popup's body** (similar to how Phase 3 renders it inside the onboarding card).

### Sub-step G — Ensure controller handles model switch from picker correctly

The controller's `UiAction::SetModel` handler already does `resolve_requested_model()`. It should continue to work if the model ID from the picker matches a catalog entry.

If the picker returns a model that was **just discovered** but isn't in the startup catalog yet, the controller may need to rebuild its catalog. Consider:

- adding the discovered model to the catalog on selection
- or triggering `reload_config()` after selection

### Sub-step H — Update help text

Update the `/help` output to indicate that `/model` now opens a picker:

```
/model [query]  — Open model picker (or switch if exact match)
```

---

## Constraints

1. **`/model` picker uses the input-replacement host from Phase 2** — not a full-screen overlay.
2. **`/model` must be blocked while a run is active.** Show a system message.
3. **Exact-match fast path must remain.** `/model gpt-4o` should switch instantly if unambiguous.
4. **Provider-management "View Models" uses the same `ModelPickerPane` component.** No duplicate picker.
5. **Selection feedback is immediate and minimal.** System message, not a success screen.
6. **Editor content is preserved** if the user had text in the editor before opening the picker.

---

## Test plan

### Required unit tests

| # | Test |
|---|------|
| 1 | `/model` with no arg opens model picker (bottom pane switches) |
| 2 | `/model exact-id` switches immediately when exact match exists |
| 3 | `/model partial` opens picker with search prefilled when no exact match |
| 4 | `/model` while streaming shows "cannot open" system message |
| 5 | `Ctrl+O` opens model picker |
| 6 | picker selection sends `UiAction::SetModel` |
| 7 | picker cancel restores editor |
| 8 | provider management "View Models" opens picker |
| 9 | picker refresh updates model list |

### Required TUI integration tests

| # | Test |
|---|------|
| 1 | `/model` render shows picker in bottom pane, transcript visible |
| 2 | `/model qwen` render shows picker with "qwen" in search field |
| 3 | help text includes updated `/model` description |

### Manual validation

1. Run with Ollama → `/model` → verify picker shows detected models
2. `/model qwen3:32b` → verify instant switch, no picker
3. `/model qw` → verify picker opens with "qw" prefilled
4. While streaming, `/model` → verify rejection message
5. `/providers` → select provider → "View Models" → verify picker opens inside popup
6. Select model from provider management → verify default changes in config

---

## Exit criteria

- [ ] `/model` opens the picker in the input-pane slot (not full-screen)
- [ ] `/model <query>` supports exact-match + prefilled-search behavior
- [ ] `Ctrl+O` opens the model picker
- [ ] picker is blocked while agent is streaming
- [ ] provider management has a "View Models" action using the same picker
- [ ] selection applies model switch and shows status message
- [ ] help text is updated
- [ ] all tests pass

---

## Follow-on phase

→ `phase_5_persistence_project_scope_and_cli_listing.md`
