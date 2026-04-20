# Plan 12 — Inline slash-command autocomplete popup

Ports pi's inline command palette to anie's TUI. Typing `/` in the
input pane opens a filterable dropdown of every registered command,
with argument-value suggestions where the command declares them.

## Motivation

### What pi does

Source walked:

- `pi/packages/tui/src/autocomplete.ts` — the provider abstraction.
- `pi/packages/tui/src/components/editor.ts:1-80, 2043-2272` — the
  editor-side state machine and rendering.
- `pi/packages/coding-agent/src/core/slash-commands.ts` — the
  metadata catalog.
- `pi/packages/coding-agent/src/modes/interactive/interactive-mode.ts:
  396-475` — how extensions and prompt templates feed the provider.

Key architectural choices worth copying:

1. **Provider interface**, not inlined logic. `AutocompleteProvider`
   has two methods: `getSuggestions(lines, cursor, opts)` returning
   `{ items, prefix }`, and `applyCompletion(lines, cursor, item,
   prefix)` returning the new buffer. The editor never builds
   suggestions itself; it renders whatever the provider returns.
2. **Two trigger contexts.** `/` at line start triggers command
   completion; space after a known command (e.g. `/model `)
   delegates to that command's `getArgumentCompletions(prefix)`.
   Same popup, different data source.
3. **SelectList popup** is rendered beneath/above the input,
   anchored to the cursor; arrow keys navigate, Enter applies,
   Tab also applies, Escape dismisses.
4. **Debounced, cancellable requests** with a per-request token so
   that racing suggestions never overwrite newer ones.
5. **"Best match" seeding.** When the popup opens, the item whose
   `value` is an exact or prefix match of what the user has typed is
   pre-selected — so pressing Enter on the first letter of a unique
   command completes it without arrow-keying.

### What anie has today

- `CommandRegistry` with name + summary (and, after plan 11,
  `argument_hint` + `arguments`).
- `InputPane` in `anie-tui::input` — a solid multi-line editor with
  history, word navigation, and grapheme-safe cursor handling. No
  popup support.
- `ModelPickerPane` (`overlays/model_picker.rs`) — a proven
  search-first picker that already occupies the bottom pane slot.
  Its filtering, scroll, and render code is the reference we'll
  extract from.
- `BottomPane` enum alternating between `Editor` and
  `ModelPicker(ModelPickerSession)`. The autocomplete popup must
  **not** replace the editor the way the model picker does — it
  overlays the editor, keeps the editor in focus, and the user
  continues typing.

### What's missing

A popup/completion-list primitive that floats above the input
without replacing it, plus the provider wiring.

## Scope

Implement the inline autocomplete popup for slash commands only,
with optional argument-value completions for commands that declare
an `Enumerated` or provider-backed spec. Do **not** implement `@`
file-path completion in this plan (roadmap #9 / a future plan
tracks that).

## Design principles

1. **Provider-first.** The popup renders items produced by a
   provider trait; the data is not hardcoded into the widget. This
   matches pi's shape and leaves room for extension-registered
   completions (plan 10).
2. **Popup is overlay, not replacement.** The editor keeps focus.
   The popup is drawn above or below the input area depending on
   vertical room. If both are cramped, the popup wins the row
   count and the editor clips to a minimum of 2 lines.
3. **Synchronous for builtin commands, async-capable for
   providers.** The builtin catalog completes instantly; the
   trait accepts an async function so `/model` can enumerate
   available models from `ModelCatalog` the same way it does in
   the full model picker.
4. **No hardcoded command names in the widget.** Every "is this a
   command line?" decision reads from the injected catalog, not
   from a match table.
5. **Keyboard behavior matches pi.** Arrow keys navigate, Enter
   applies + inserts trailing space for the next argument, Tab
   cycles suggestions (or applies if exactly one remains), Escape
   dismisses, typing continues filtering.
6. **Add-a-command friction stays tiny.** Registering a new
   command in the builtin catalog (or, later, via extension)
   should automatically make it appear in the popup without any
   additional TUI plumbing.

---

## Phase A — Extract a shared `SelectList` widget

**Goal:** A reusable scrollable list primitive with a filtered
view, used both by the autocomplete popup (this plan) and
(eventually) by the model picker and session selectors.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/widgets/select_list.rs` (new) | Extract the scroll + filter + render state machine from `overlays/model_picker.rs`. |
| `crates/anie-tui/src/widgets/mod.rs` | Export `SelectList`. |
| `crates/anie-tui/src/widgets/select_list.rs` (tests) | Unit tests: filter, wrap-around navigation, scroll-into-view. |

### Sub-step A — Minimal public API

```rust
pub(crate) struct SelectList<T> {
    items: Vec<T>,
    filtered: Vec<usize>,  // indices into `items`
    selected: usize,       // index into `filtered`
    scroll: usize,
    max_visible: usize,
}

impl<T> SelectList<T> {
    pub fn new(items: Vec<T>, max_visible: usize) -> Self { ... }
    pub fn set_items(&mut self, items: Vec<T>);
    pub fn apply_filter(&mut self, predicate: impl Fn(&T) -> bool);
    pub fn move_selection(&mut self, delta: isize);  // wraps
    pub fn set_selected_value(&mut self, predicate: impl Fn(&T) -> bool);
    pub fn selected(&self) -> Option<&T>;
    pub fn visible(&self) -> impl Iterator<Item = (usize, &T, bool /* selected */)>;
    pub fn height_hint(&self) -> u16;  // actual, clamped to max_visible
}
```

### Sub-step B — Scope of extraction

Do not refactor `ModelPickerPane` to use `SelectList` in this phase
(separate change, separate review). Just **extract** the shared
state machine into the new module and cover it with unit tests.
The `ModelPickerPane` migration can be a follow-up once the widget
has shipped and proven itself.

### Test plan

| # | Test |
|---|---|
| 1 | `select_list_filter_narrows_visible_set` |
| 2 | `select_list_move_selection_wraps_at_bounds` |
| 3 | `select_list_scroll_keeps_selection_visible_when_max_visible_small` |
| 4 | `select_list_set_items_preserves_selection_when_possible` |
| 5 | `select_list_apply_filter_resets_selection_to_first_match` |
| 6 | `select_list_height_hint_clamps_to_max_visible` |

### Exit criteria

- [ ] `SelectList<T>` lives under `anie-tui/src/widgets/`.
- [ ] Six unit tests pass.
- [ ] No existing behavior regresses (the model picker still uses
      its own copy).

---

## Phase B — `AutocompleteProvider` trait + builtin provider

**Goal:** Define the provider contract and implement the builtin
slash-command completer on top of the catalog from plan 11.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/autocomplete/mod.rs` (new) | `AutocompleteProvider` trait, `Suggestion`, `SuggestionSet`, and the parser for `/name [arg]` contexts. |
| `crates/anie-tui/src/autocomplete/command.rs` (new) | `CommandCompletionProvider` that reads `SlashCommandInfo` (shipped by plan 11 phase B) and yields suggestions. |
| `crates/anie-tui/src/autocomplete/mod.rs` (tests) | Suggestion-shape and parsing tests. |
| `crates/anie-tui/src/lib.rs` | Export the new module. |

### Sub-step A — Data types

```rust
/// A single suggestion row rendered in the popup.
#[derive(Debug, Clone)]
pub struct Suggestion {
    /// The text inserted when the user accepts.
    pub value: String,
    /// The label shown in the popup (e.g. "thinking").
    pub label: String,
    /// Optional second column (e.g. "[off|low|medium|high]" or a
    /// short description). Truncated at render time.
    pub description: Option<String>,
}

/// A suggestion batch plus the text slice it replaces.
#[derive(Debug, Clone)]
pub struct SuggestionSet {
    pub items: Vec<Suggestion>,
    /// The substring of the input line that will be replaced when
    /// `value` is applied. Typically `/xyz` at the line start, or
    /// the argument prefix after a command name.
    pub prefix: String,
    /// Whether this is a command-name context or an argument
    /// context. Affects cursor positioning after apply.
    pub kind: SuggestionKind,
}

pub enum SuggestionKind {
    CommandName,
    ArgumentValue { command_name: String },
}
```

### Sub-step B — Trait

```rust
#[async_trait::async_trait]
pub trait AutocompleteProvider: Send {
    async fn suggestions(
        &self,
        line: &str,
        cursor: usize,
    ) -> Option<SuggestionSet>;
}
```

(If we want to avoid `async_trait` macros, we can use a
`Pin<Box<dyn Future>>` return or provide a synchronous
`CommandCompletionProvider` now and defer async to when a
provider genuinely needs it. Decide at implementation time; the
trait shape is what matters for this plan.)

### Sub-step C — Context parsing

Mirror pi's `autocomplete.ts` logic:

```
Input: "   /thinking me"     cursor at end
  → kind = ArgumentValue { command_name: "thinking" }
    prefix = "me"
Input: "/thi"                 cursor at end
  → kind = CommandName
    prefix = "/thi"
Input: "not a command /foo"  cursor at end
  → None  (only recognize `/` at line start)
Input: ""
  → None
```

The parser is a pure function on `(&str, usize)`; test it
exhaustively before wiring anything to the UI.

### Sub-step D — `CommandCompletionProvider`

```rust
pub struct CommandCompletionProvider {
    commands: Vec<SlashCommandInfo>,
    argument_sources: HashMap<String, Box<dyn ArgumentSource>>,
}

pub trait ArgumentSource: Send + Sync {
    fn completions(&self, prefix: &str) -> Vec<Suggestion>;
}
```

For builtin commands:

- `ArgumentSpec::Enumerated { values, .. }` → a `StaticValuesSource`
  returning `values.iter().filter(|v| v.starts_with(prefix))`.
- `ArgumentSpec::FreeForm` → no argument source by default. Plan 12
  ships **no** dynamic argument source for `/model` — models are
  routed through the existing picker on Enter. If we later want
  `/model <fuzzy>` completions, add a `ModelArgumentSource` in a
  follow-up plan.
- `ArgumentSpec::Subcommands { known }` → a `StaticValuesSource` over
  `known`.
- `ArgumentSpec::None` → no argument source.

### Test plan

| # | Test |
|---|---|
| 1 | `parse_context_command_name_at_line_start` |
| 2 | `parse_context_returns_none_if_slash_not_at_line_start` |
| 3 | `parse_context_argument_after_known_command` |
| 4 | `parse_context_argument_empty_prefix_when_cursor_after_space` |
| 5 | `command_provider_returns_all_commands_for_empty_prefix` |
| 6 | `command_provider_filters_by_startswith_case_insensitive` |
| 7 | `command_provider_thinking_argument_returns_four_values` |
| 8 | `command_provider_session_subcommand_returns_list_and_known_ids` (if we decide to surface session IDs; otherwise return only `list`) |
| 9 | `command_provider_none_when_cursor_inside_freeform_model_arg` (confirms we deliberately don't complete model IDs in the popup in this plan) |

### Exit criteria

- [ ] Parser covers command-name and argument contexts with tests.
- [ ] `CommandCompletionProvider` ships with argument completions
      for every `Enumerated` builtin.
- [ ] Zero hardcoded command names inside the provider —
      everything drives from `SlashCommandInfo`.

---

## Phase C — Popup rendering

**Goal:** An overlay that floats above the input, shows the
current `SuggestionSet`, and stays in sync with editor state.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/autocomplete/popup.rs` (new) | `AutocompletePopup` widget: wraps `SelectList<Suggestion>`, renders a bordered box with two columns (label, description). |
| `crates/anie-tui/src/app.rs` | Thread the popup through `App`: construction, render, size calculation, keyboard passthrough. |
| `crates/anie-tui/src/app.rs` (layout) | Compute popup rect above the input pane when there's room; below otherwise. Ensure the editor retains ≥3 lines. |

### Sub-step A — `AutocompletePopup`

```rust
pub(crate) struct AutocompletePopup {
    list: SelectList<Suggestion>,
    prefix: String,
    kind: SuggestionKind,
    max_width: u16,
    max_visible: u16,
}
```

Render contract:

- Title bar: `"commands"` or `"<command-name> values"` depending
  on `kind`.
- Each row: `{label:<20}  {description}` truncated to
  `max_width - 4`.
- Border color: accent. Selected row highlight matches the model
  picker's existing style.
- Uses `Clear` before drawing to blank out any underlying cells.

### Sub-step B — Layout

In `App::render`:

1. Compute normal `bottom_pane` height as today.
2. If `self.autocomplete.is_some()`:
   - Compute popup height = `min(max_visible, list.height_hint(),
     available_rows_above_input)`.
   - Reserve that height above the input.
   - Render popup into the reserved rect.
3. Else: no change.

If there isn't room above (e.g., the input is flush against the
top of the terminal), render **below** the input. If there isn't
room anywhere, don't render the popup (and cancel autocomplete).

### Sub-step C — Focus

The input pane remains focused. The popup is a purely visual
overlay; all keys continue to go to the editor, which either
consumes them or forwards them (see phase D).

### Test plan

| # | Test |
|---|---|
| 1 | `popup_renders_commands_with_labels_and_descriptions` (TestBackend frame snapshot) |
| 2 | `popup_renders_above_input_when_space_available` |
| 3 | `popup_renders_below_input_when_top_is_cramped` |
| 4 | `popup_selected_row_uses_highlight_style` |
| 5 | `popup_truncates_long_descriptions_with_ellipsis` |
| 6 | `popup_scroll_keeps_selected_in_viewport_when_max_visible_small` |

### Exit criteria

- [ ] Popup renders correctly in TestBackend snapshots.
- [ ] Layout respects terminal bounds and input-area minimum.
- [ ] No z-order or overdraw artifacts in `Clear`-covered cells.

---

## Phase D — Editor integration (triggering, filtering, applying)

**Goal:** Typing `/` opens the popup; typing filters it;
arrow/Enter/Tab/Escape work; editing after the cursor keeps the
popup in sync.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/input.rs` | Add `AutocompleteState` field to `InputPane`; key routing that consults the state; hooks after every buffer mutation to update suggestions. |
| `crates/anie-tui/src/app.rs` | Plumb a provider handle (from `App::new`) into `InputPane`; render the popup in `render`. |
| `crates/anie-tui/src/lib.rs` | Export the necessary types for the CLI wiring. |
| `crates/anie-cli/src/interactive_mode.rs` | Construct the provider, pass it to `App::new`. |

### Sub-step A — Provider handle plumbing

```rust
pub struct InputPane {
    // existing fields...
    autocomplete: Option<AutocompleteRuntime>,
}

struct AutocompleteRuntime {
    provider: Arc<dyn AutocompleteProvider>,
    popup: Option<AutocompletePopup>,
    last_query: Option<String>,
    request_token: u64,
}

impl InputPane {
    pub fn with_autocomplete(mut self, provider: Arc<dyn AutocompleteProvider>) -> Self {
        self.autocomplete = Some(AutocompleteRuntime { ... });
        self
    }
}
```

If the CLI doesn't install a provider (unlikely; it should), the
editor behaves exactly as it does today.

### Sub-step B — Trigger rules

After every mutating keypress (`insert_char`, `backspace`,
`delete`, cursor move), call `update_autocomplete()`:

```rust
fn update_autocomplete(&mut self) {
    let Some(rt) = self.autocomplete.as_mut() else { return };
    let query = (self.content.clone(), self.cursor);
    if Some(&query.0) == rt.last_query.as_ref() {
        return;  // cursor-only move with same content: no re-query
    }
    rt.last_query = Some(query.0.clone());
    rt.request_token = rt.request_token.wrapping_add(1);
    let token = rt.request_token;
    let provider = Arc::clone(&rt.provider);
    // Spawn the request; on completion, ignore if token has moved.
    // For the synchronous builtin provider we can call inline:
    let suggestions = provider.suggestions_sync(&query.0, query.1);
    if token != rt.request_token { return; }
    self.apply_suggestions(suggestions);
}
```

`provider.suggestions_sync` is a sibling method on the trait; the
default impl can block on the async version. The builtin command
provider overrides `suggestions_sync` directly because it's pure
CPU.

### Sub-step C — Key routing

When the popup is open, intercept these keys in
`InputPane::handle_key` **before** the normal editor dispatch:

| Key | Action |
|---|---|
| `Up` / `Ctrl+P` | Move selection up (wraps). |
| `Down` / `Ctrl+N` | Move selection down (wraps). |
| `Enter` | Apply the selected suggestion. Replace `prefix` with `value`, insert a trailing space when `kind == CommandName`, close popup. Then let the editor's normal Enter handling fire **only if** the command has no argument spec (i.e., the completion finished the invocation). In practice: after applying a `CommandName` suggestion, the popup may re-trigger as an `ArgumentValue` context if the command takes arguments; otherwise Enter continues into the editor's normal submit. (Mirror pi's behavior: Enter always applies the completion and never submits when popup is visible.) |
| `Tab` | Same as Enter, but never submits. |
| `Escape` | Dismiss popup, do not modify buffer. |
| Any other key | Forward to editor; popup re-queries after the mutation. |

Up/Down history navigation is preempted while the popup is open —
but only then. When the popup is closed, the existing history
behavior is unchanged.

### Sub-step D — Apply logic

```rust
fn apply_suggestion(&mut self, suggestion: Suggestion, set: &SuggestionSet) {
    let start = self.cursor.saturating_sub(set.prefix.len());
    self.content.replace_range(start..self.cursor, &suggestion.value);
    self.cursor = start + suggestion.value.len();
    match set.kind {
        SuggestionKind::CommandName => {
            // Append a trailing space unless the next char is already space.
            let next = self.content.as_bytes().get(self.cursor).copied();
            if next != Some(b' ') {
                self.content.insert(self.cursor, ' ');
                self.cursor += 1;
            }
        }
        SuggestionKind::ArgumentValue { .. } => { /* no trailing space */ }
    }
    self.close_autocomplete();
}
```

After applying a `CommandName`, immediately call
`update_autocomplete()` — if the command accepts arguments, this
will open the argument-value popup.

### Test plan

| # | Test (in `anie-tui/src/tests.rs`) |
|---|---|
| 1 | `typing_slash_opens_popup_with_all_commands` |
| 2 | `typing_filter_narrows_popup_to_matching_commands` |
| 3 | `enter_on_command_name_inserts_name_and_trailing_space` |
| 4 | `typing_slash_thinking_space_opens_enumerated_popup` |
| 5 | `enter_on_enumerated_value_replaces_prefix_without_trailing_space` |
| 6 | `escape_dismisses_popup_without_modifying_buffer` |
| 7 | `arrow_keys_navigate_popup_and_skip_history_while_open` |
| 8 | `backspace_reopens_popup_with_updated_filter` |
| 9 | `popup_closes_when_command_fully_formed_then_submit_dispatches_uiaction` |
| 10 | `popup_does_not_open_for_non_slash_input` |
| 11 | `popup_does_not_open_when_slash_is_not_at_line_start` |

### Exit criteria

- [ ] Popup opens on `/` at line start; closes on Esc, blank line,
      or accepted command.
- [ ] All 11 integration tests pass in `anie-tui/src/tests.rs`.
- [ ] `/thinking m<Tab>` completes to `/thinking medium` without
      needing plan 11's fallback error path.
- [ ] `/` then `mo<Enter>` submits `/model` (inserts name + space,
      waits for user to decide between arg and submit).

---

## Phase E — Argument-source extensibility + wiring polish

**Goal:** Ship the remaining bits needed before closing out roadmap
item #7: extension seam, `/help` menu synergy, settings toggle,
and docs.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/autocomplete/command.rs` | Accept `Box<dyn ArgumentSource>` per command at construction; `CommandCompletionProvider::new(commands, sources)`. |
| `crates/anie-cli/src/interactive_mode.rs` | Register the default argument sources (enumerated) and any dynamic ones we decide to ship (e.g., `ModelArgumentSource` if plan 12 phase D doesn't cover it). |
| `crates/anie-config/src/lib.rs` | New boolean setting `ui.slash_command_popup_enabled` (default true). |
| `crates/anie-tui/src/app.rs` | Respect the setting when constructing `InputPane`. |
| `crates/anie-tui/src/tests.rs` | Test: disabled setting means no popup opens on `/`. |
| `docs/notes/commands_and_slash_menu.md` | Update "Current State" to reflect that autocomplete has landed; move the action-item list to "Follow-ups" (e.g., file-path `@` completion). |
| `docs/ROADMAP.md` | Check off item #7. Add a follow-up entry for file-path completion if appropriate. |

### Sub-step A — `/help` synergy

After plan 11 phase B, `/help` already renders `argument_hint`
inline. No further change here, but cross-test that the two sources
of truth (popup description column and `/help` output) both read
from `SlashCommandInfo` — not from parallel tables.

### Sub-step B — Extension-command path (forward-compat)

Plan 10 will register extension commands into the registry. The
autocomplete pipeline should Just Work because it reads
`SlashCommandInfo` indiscriminately. This phase adds exactly **one**
test that proves it:

```rust
#[test]
fn popup_includes_extension_registered_command() {
    let mut registry = CommandRegistry::with_builtins();
    registry.register(SlashCommandInfo {
        name: "ext-foo",
        summary: "Test extension command",
        source: SlashCommandSource::Extension { extension_name: "x".into() },
        arguments: ArgumentSpec::None,
        argument_hint: None,
    }).unwrap();
    let provider = CommandCompletionProvider::from_registry(&registry);
    let set = provider.suggestions_sync("/ext-", 5).unwrap();
    assert!(set.items.iter().any(|s| s.label == "ext-foo"));
}
```

This test guards the contract that plan 10 depends on.

### Sub-step C — Settings toggle

A user who dislikes the popup should be able to disable it in
`~/.anie/config.toml`:

```toml
[ui]
slash_command_popup_enabled = false
```

Default is `true`. The setting is read once at startup; reloading
the config (`/reload`) rebuilds `InputPane`.

### Sub-step D — README + docs

Update these user-facing docs:

- `README.md` — mention the popup under "Interactive mode".
- `docs/notes/commands_and_slash_menu.md` — mark action item #1
  done; trim action items to "remaining."
- `docs/ROADMAP.md` — move item #7 to "Completed."

### Test plan

| # | Test |
|---|---|
| 1 | `disabled_setting_prevents_popup` |
| 2 | `extension_registered_command_appears_in_popup` |
| 3 | `help_and_popup_render_same_argument_hint_for_thinking` |

### Exit criteria

- [ ] Extension-registered commands appear in the popup with
      only the same registration call used by builtins.
- [ ] `ui.slash_command_popup_enabled = false` hides the popup
      without hiding the commands from `/help`.
- [ ] ROADMAP item #7 checked off.

---

## Phase ordering

A → B → C → D → E. Each phase is reviewable in one sitting.

Phase A is low-risk and ships on its own. Phase B is pure logic
with no UI. Phase C adds rendering with no editor coupling. Phase
D is the only phase that touches the hot path of user input.
Phase E is polish + docs.

## Out of scope

- `@path` file-path completion (use pi's `CombinedAutocompleteProvider`
  as the reference when we write that plan).
- `$env` / variable completion.
- Persisting popup state across sessions.
- Model-argument dynamic completion for `/model` (follow-up).
- Refactoring `ModelPickerPane` onto `SelectList` (separate PR
  after phase A lands).

## Risks

1. **Input latency.** The popup re-queries after every keystroke.
   For the builtin provider the work is microseconds; document
   the expectation that dynamic providers stay fast or offload to
   a background task. The request-token design in phase D keeps
   racing providers from corrupting UI state.
2. **Keybinding collisions.** Enter inside an open popup behaves
   differently from Enter in the editor. Phase D test #9 pins this
   down explicitly.
3. **Rect math at small terminal sizes.** If the terminal is very
   short, the popup may not render at all. The layout logic must
   degrade gracefully (no rendering is better than rendering over
   the input). Tests in phase C cover this.
4. **Async-trait bootstrap.** If we take the async-trait route for
   future dynamic providers, that's a new dependency. Call it out
   in the PR; alternatively ship synchronous-only in this plan
   and add async at the first provider that needs it.

## Success metric

After both plans 11 and 12 land, the following interaction works
with zero typo-induced lockups and zero memorization required:

```
User types: /
  → popup lists all 15 builtin commands.
User types: th
  → popup filters to /thinking (highlighted).
User presses Enter:
  → input becomes "/thinking ", popup re-opens showing
    off/low/medium/high.
User types: h
  → popup filters to /thinking high (highlighted).
User presses Enter:
  → input becomes "/thinking high", popup closes.
User presses Enter again:
  → command dispatches; status bar updates to "high".
```

No point in that sequence can a typo produce a broken controller.
