# Phase 2 — Pi-Style Selector Host and Model Picker

This phase adds the core interactive model-picker UI using a pi-style **input-replacement** pattern. The picker appears in the area normally occupied by the input editor — the transcript and status bar stay visible.

## Why this phase exists

This is the most user-visible change in the feature set. Everything that follows — onboarding integration, `/model`, provider browsing — is a **consumer** of this shared picker component.

The design is derived directly from pi's `showSelector(...)` + `ModelSelectorComponent` pattern.

---

## Pi reference: how `showSelector(...)` works

In pi's `interactive-mode.ts`:

```typescript
private showSelector(create) {
    const done = () => {
        this.editorContainer.clear();
        this.editorContainer.addChild(this.editor);
        this.ui.setFocus(this.editor);
    };
    const { component, focus } = create(done);
    this.editorContainer.clear();
    this.editorContainer.addChild(component);
    this.ui.setFocus(focus);
}
```

Key properties:

- the selector **replaces the editor**, not the whole screen
- `done()` restores the editor when the selector closes
- the transcript and footer remain visible throughout
- each selector provides its own `handleInput(...)` and `render(...)`

In pi's `ModelSelectorComponent`:

- bordered container with top/bottom borders
- search input at the top (focused by default)
- scrollable list of max ~10 visible rows
- provider badge per row (e.g. `[openai]`)
- current-model checkmark marker
- footer hints for navigation

---

## Current Anie code facts

### TUI layout (`crates/anie-tui/src/app.rs`)

```rust
fn layout(area: Rect, input_height: u16) -> (Rect, Rect, Rect) {
    // output_area, status_area, input_area
    // input_area height is input_pane.preferred_height().clamp(3, 8)
}
```

The bottom pane is currently **always** the `InputPane`.

### Current overlay system

`App` has an `overlay: Option<OverlayState>` that renders over the **entire frame**:

```rust
enum OverlayState {
    Onboarding(OnboardingScreen),
    Providers(ProviderManagementScreen),
}
```

This is used for `/onboard` and `/providers`. It is **not** the right mechanism for `/model` — that would be full-screen, exactly what we want to avoid.

### Current `/model` handling

```rust
"/model" => match arg {
    None => /* print current model info */,
    Some(model_id) => /* send UiAction::SetModel(model_id) */,
}
```

No picker today. Just text-in, action-out.

---

## Files expected to change

### New files

- `crates/anie-tui/src/model_picker.rs` — the reusable `ModelPickerPane` component

### Modified files

- `crates/anie-tui/src/app.rs` — add `BottomPane` enum, selector host lifecycle, layout changes
- `crates/anie-tui/src/lib.rs` — export `ModelPickerPane` and action types
- `crates/anie-tui/src/tests.rs` — selector host regression tests

### Not yet

- `crates/anie-tui/src/onboarding.rs` — Phase 3
- `crates/anie-tui/src/providers.rs` — Phase 4
- `crates/anie-cli/` — Phase 4

---

## Recommended implementation

### Sub-step A — Add `BottomPane` enum to `App`

Replace the hard-wired `input_pane` rendering with a switchable bottom pane:

```rust
enum BottomPane {
    /// Normal text input editor.
    Editor,
    /// Model picker selector (pi-style).
    ModelPicker(ModelPickerPane),
}
```

The `App` struct keeps `input_pane: InputPane` as a permanent field (its content is preserved while a picker is open). The `bottom_pane: BottomPane` field controls which widget is currently rendered and receives key events.

When `bottom_pane` is `Editor`:

- render and focus `input_pane` as today
- height is `input_pane.preferred_height().clamp(3, 8)`

When `bottom_pane` is `ModelPicker(picker)`:

- render and focus the picker
- height is `picker.preferred_height().clamp(8, area.height / 2)`

### Sub-step B — Adjust layout for dynamic bottom-pane height

Update the `layout()` function to accept the bottom-pane height rather than always computing it from `InputPane`:

```rust
fn layout(area: Rect, bottom_height: u16) -> (Rect, Rect, Rect) {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),          // output pane
            Constraint::Length(1),        // status bar
            Constraint::Length(bottom_height),
        ])
        .split(area)
}
```

In `App::render()`:

```rust
let bottom_height = match &self.bottom_pane {
    BottomPane::Editor => self.input_pane.preferred_height(width).clamp(3, 8),
    BottomPane::ModelPicker(picker) => picker.preferred_height(width).clamp(8, area.height / 2),
};
```

### Sub-step C — Implement `ModelPickerPane`

Create `crates/anie-tui/src/model_picker.rs`.

#### Public API

```rust
pub struct ModelPickerPane { ... }

pub enum ModelPickerAction {
    /// Keep rendering the picker.
    Continue,
    /// User selected a model.
    Selected(ModelInfo),
    /// User cancelled (Esc).
    Cancelled,
    /// User requested a model list refresh (r key).
    Refresh,
}

impl ModelPickerPane {
    /// Create a new picker with an initial model list.
    pub fn new(
        models: Vec<ModelInfo>,
        current_provider: String,
        current_model_id: String,
        initial_search: Option<String>,
    ) -> Self;

    /// Replace the model list (after a refresh or provider change).
    pub fn set_models(&mut self, models: Vec<ModelInfo>);

    /// Set loading state (shown while refreshing).
    pub fn set_loading(&mut self, loading: bool);

    /// Set an error message (shown inline below the list).
    pub fn set_error(&mut self, error: Option<String>);

    /// Preferred height in rows (borders + search + list + footer).
    pub fn preferred_height(&self, width: u16) -> u16;

    /// Render the picker into the provided area.
    pub fn render(&mut self, area: Rect, buf: &mut ratatui::buffer::Buffer, spinner_frame: &str);

    /// Handle a key event.
    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPickerAction;
}
```

#### Internal layout

```
┌─ Select Model ──────────────────────────────────────────────┐
│ Search: qwen_                                               │
│                                                             │
│ › qwen3:32b                            [ollama]  ✓          │
│   qwen3:8b                             [ollama]             │
│   qwen3:1.7b                           [ollama]             │
│                                              (1/3)          │
│                                                             │
│ [↑↓] Navigate  [Enter] Select  [r] Refresh  [Esc] Cancel   │
└─────────────────────────────────────────────────────────────┘
```

- **Title**: "Select Model" or "Select Model — {provider}"
- **Search input**: single-line, focused by default, filters the list as you type
- **Model list**: max 8–10 visible rows, scrollable, highlight with `›` prefix
- **Row content**: model ID, provider badge `[provider]`, current-model marker `✓`
- **Scroll indicator**: `(N/M)` when list is longer than visible area
- **Loading state**: replace list with spinner + "Discovering models…"
- **Error state**: show error message in red below the search input
- **Footer**: context-sensitive keybinding hints

#### Search behavior

- substring match on `model.id` and `model.name`
- case-insensitive
- preserve selection position sanely as results change (keep selected item visible if it still matches, otherwise reset to first)

#### Key handling

| Key | Action |
|-----|--------|
| printable chars / backspace / delete | edit search field |
| `↑` / `k` | move selection up (wrap to bottom) |
| `↓` / `j` | move selection down (wrap to top) |
| `Enter` | select current item → `ModelPickerAction::Selected(...)` |
| `Esc` | cancel → `ModelPickerAction::Cancelled` |
| `r` | refresh → `ModelPickerAction::Refresh` |
| `Home` | jump to first item |
| `End` | jump to last item |

### Sub-step D — Wire the selector host lifecycle in `App`

Add methods to `App`:

```rust
/// Open the model picker, replacing the editor.
fn open_model_picker(&mut self, models: Vec<ModelInfo>, initial_search: Option<String>);

/// Close the model picker and restore the editor.
fn close_model_picker(&mut self);

/// Handle a key event routed to the model picker.
fn handle_model_picker_key(&mut self, key: KeyEvent);
```

In `handle_terminal_event()`, when `bottom_pane` is `ModelPicker`:

- route key events to the picker
- on `Selected` → emit `UiAction::SetModel(...)`, close picker, show status message
- on `Cancelled` → close picker silently
- on `Refresh` → spawn model discovery task, update picker with results

In `render()`, delegate to the correct bottom pane.

### Sub-step E — Add TUI tests

Cover these scenarios:

1. **picker replaces input pane, transcript stays visible**
   - open picker → render → verify output pane content still present
   - verify status bar still present
   - verify input pane is not rendered

2. **search input filters the list**
   - type characters → verify visible items change

3. **selection emits correct action**
   - navigate + enter → verify `UiAction::SetModel` is sent with correct model

4. **cancel restores editor**
   - open picker → Esc → verify input pane is back

5. **editor content is preserved across open/close**
   - type text in editor → open picker → cancel → verify text still in editor

6. **small terminal renders safely**
   - 40×12 terminal → picker still usable

---

## Constraints

1. **The picker must not be full-screen.** It replaces the input pane only.
2. **The transcript must remain visible** while the picker is open.
3. **The status bar must remain visible** while the picker is open.
4. **The overlay system** (`OverlayState`) is not used for this. That stays for onboarding/provider management.
5. **Editor state is preserved.** Opening/closing the picker must not lose draft input text.
6. **The picker must be reusable.** Onboarding (Phase 3), `/model` (Phase 4), and provider-management "View Models" (Phase 4) all use the same component.

---

## Test plan

### Required unit tests

| # | Test |
|---|------|
| 1 | picker preferred_height returns correct value for various list sizes |
| 2 | search filters models by substring match on id and name |
| 3 | empty search shows all models |
| 4 | no-results state renders cleanly |
| 5 | selection wraps at list boundaries |
| 6 | Enter on selected item produces `Selected(...)` |
| 7 | Esc produces `Cancelled` |
| 8 | `r` produces `Refresh` |
| 9 | set_loading shows spinner state |
| 10 | set_error shows inline error |
| 11 | current-model marker renders on the correct row |

### Required TUI integration tests

| # | Test |
|---|------|
| 1 | opening picker keeps transcript visible in rendered output |
| 2 | closing picker restores input pane with preserved content |
| 3 | selection sends `UiAction::SetModel` |

### Manual validation

1. with Ollama running and 5+ models, open picker and verify scrolling works
2. type a search term and verify filtering is instant
3. select a model and verify footer/status bar updates
4. cancel and verify editor draft text is preserved

---

## Exit criteria

- [ ] `ModelPickerPane` exists as a reusable component
- [ ] it renders in the input-pane region (not full-screen)
- [ ] search, selection, cancel, refresh all work
- [ ] transcript and status bar remain visible while picker is open
- [ ] editor content is preserved across open/close
- [ ] TUI tests cover the selector host behavior
- [ ] no onboarding, provider-management, or `/model` behavior changed yet

---

## Follow-on phase

→ `phase_3_onboarding_inline_model_selection.md`
