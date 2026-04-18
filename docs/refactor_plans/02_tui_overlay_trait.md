# Plan 02 — TUI overlay trait + shared widgets

> **Revised 2026-04-17.** Extended with Phase 6 to establish an
> `overlays/` directory scaffolded for the 10+ future selectors
> pi-mono has (`session_picker`, `theme_picker`, `settings`,
> `oauth`, `login`, `hotkeys`, etc.). See `pi_mono_comparison.md`
> for the pi inventory. Directory scaffolding is cheap and
> prevents re-migrating overlays as they're added.

> **Status (2026-04-17):** All structural phases landed on
> `refactor_branch`.
> - **Phase 1 (TextField):** `d47fbcc`. 15 unit tests.
> - **Phase 2 (panel helpers):** `69adfe2`. 4 unit tests.
> - **Phase 3 (OverlayScreen trait + App migration):** `32b68cf`.
>   Flat-union `OverlayOutcome` (option A) chosen. Inherent
>   methods on screens retained so existing tests are
>   undisturbed; trait methods are `dispatch_*` to sidestep
>   name collisions.
> - **Phase 4 (clone audit):** Largest items landed in the
>   plan 00 followup (commit `107a840`). Remaining low-impact
>   items (render-path `self.state.clone()` in
>   `onboarding::render`; `HashMap<String, TestResult>` in
>   providers) deferred — behind cost/benefit: render clone is
>   once per frame on a small enum; provider-map typo risk is
>   contained to a small surface.
> - **Phase 5 (overlay tests):** Deferred. The two overlays
>   still have no direct tests, but the trait now makes them
>   easier to write. Trading off against moving plans 03–05
>   forward.
> - **Phase 6 (overlays directory):** `b4cb615`. `onboarding`,
>   `providers`, `model_picker` now under
>   `crates/anie-tui/src/overlays/`. Future screens (settings,
>   login, tree, theme picker) land here via `impl
>   OverlayScreen`.

## Motivation

`crates/anie-tui/src/onboarding.rs` (2312 LOC) and
`crates/anie-tui/src/providers.rs` (1432 LOC) are parallel
implementations of "full-screen overlay that configures providers."
They have independently diverged and now duplicate:

- `struct TextField` and its impl (onboarding.rs:169 + impl at 1548;
  providers.rs:120 + impl at 1241).
- Rendering helpers `centered_rect`, `footer_line`,
  `previous_boundary`, `next_boundary` (onboarding.rs:1999–2044 and
  providers.rs:1302–1347).
- Status/busy panel rendering (`render_status_panel`,
  `render_busy_panel`) duplicated by shape.
- Model-picker embedding code that wraps `ModelPickerPane` with
  near-identical overlay chrome.

`crates/anie-tui/src/app.rs` holds `enum OverlayState` and matches on
it twice — once for event dispatch, once for render. Adding a third
overlay (e.g., `/settings` from `docs/ideas.md`) currently requires
editing:

1. The enum.
2. The event dispatch match in `App`.
3. The render match in `App`.
4. A new monolithic file that copies the onboarding/providers
   scaffolding a third time.

Additionally, `providers.rs:132` keys `test_results` by
`HashMap<String, TestResult>` on the raw provider name — typo-prone.
And `OnboardingState::clone()` is called on every tick/render cycle
(`onboarding.rs:233, 284`), which is both wasteful and a signal the
borrow shape is fighting state ownership.

## Design principles

1. **One `OverlayScreen` trait.** All full-screen overlays implement
   it. `App` holds `Option<Box<dyn OverlayScreen>>` instead of an
   enum.
2. **Shared widgets live in `anie-tui::widgets`.** `TextField`,
   `render_status_panel`, `render_busy_panel`, `centered_rect`,
   `footer_line`, boundary helpers — all in one module.
3. **State enums stay internal.** Each overlay's internal state
   machine (e.g., `OnboardingState`) stays inside its module. Only
   the trait is shared.
4. **No behavioral change visible to users.** Same keys, same flows,
   same rendering. This is cleanup, not redesign.
5. **Tests land alongside the extraction.** `TextField` and the
   overlay transitions become unit-testable for the first time.

## Current file layout (verified 2026-04-17)

`onboarding.rs`:

| Lines | Contents |
|---|---|
| 27–176 | Public types (`ConfiguredProvider`, `OnboardingAction`, etc.) |
| 66–108 | `enum OnboardingState` (11 variants) |
| 169–175 | `struct TextField` declaration |
| 176–1453 | `OnboardingScreen` + impl |
| 1454–1546 | Helper enums (`MainMenuItem`, `ModelPickerContext`, `CustomEndpointForm`) |
| 1548–1655 | `impl TextField` |
| 1657–1997 | Provider preset / validation / discovery helpers |
| 1999–2044 | `centered_rect`, `footer_line`, boundary helpers |

`providers.rs`:

| Lines | Contents |
|---|---|
| 32–126 | Public types + internal enums |
| 120–126 | `struct TextField` declaration |
| 127–955 | `ProviderManagementScreen` + impl |
| 956–1240 | Action items / entry loading / validation helpers |
| 1241–1300 | `impl TextField` |
| 1302–1347 | `centered_rect`, `footer_line`, boundary helpers |

---

## Phase 1 — Extract `TextField` to `widgets::text_field`

**Goal:** One `TextField` implementation, used from both overlays.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/widgets/mod.rs` | New — declares `pub mod text_field;` and re-exports `TextField` |
| `crates/anie-tui/src/widgets/text_field.rs` | New — single `struct TextField` with merged impl + unit tests |
| `crates/anie-tui/src/lib.rs` | Add `pub mod widgets;` (or keep the module crate-private if no external consumers) |
| `crates/anie-tui/src/onboarding.rs` | Remove local `TextField`; `use crate::widgets::TextField;` |
| `crates/anie-tui/src/providers.rs` | Remove local `TextField`; `use crate::widgets::TextField;` |

### Sub-step A — Pick the source of truth

Diff the two `TextField` impls. Pick whichever has better coverage of
UTF-8 boundaries and masking. If they differ in any meaningful way,
document the chosen behavior in a comment at the top of the new
file. (Acceptable outcomes: whichever variant was already in
`onboarding.rs` wins, because the onboarding flow exercises more
edge cases.)

### Sub-step B — Add unit tests

| # | Test |
|---|------|
| 1 | `insert_char_at_end_moves_cursor` |
| 2 | `backspace_deletes_previous_grapheme` (NOT just previous byte) |
| 3 | `left_arrow_respects_grapheme_boundary` |
| 4 | `right_arrow_respects_grapheme_boundary` |
| 5 | `masked_value_rendered_with_mask_char` (if masking is supported) |
| 6 | `ctrl_a_moves_to_start` / `ctrl_e_moves_to_end` (match current bindings) |
| 7 | `home_end_work_as_expected` |
| 8 | `paste_multiline_strips_newlines` (if applicable; pin current behavior) |

### Files that must NOT change

- `crates/anie-tui/src/input.rs` — the main input editor is not
  `TextField`; leave it alone.
- `crates/anie-tui/src/model_picker.rs` — its search field is its
  own concern; consider migrating in a later phase.

### Exit criteria

- [ ] One `TextField`, one impl.
- [ ] Both overlay files import from `widgets`.
- [ ] 6+ unit tests pass.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.

---

## Phase 2 — Extract shared render helpers to `widgets::panel`

**Goal:** One place for status-panel / busy-panel / centered-rect /
footer-line / boundary helpers.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/widgets/panel.rs` | New — `render_status_panel`, `render_busy_panel`, `centered_rect`, `footer_line`, `previous_boundary`, `next_boundary` |
| `crates/anie-tui/src/widgets/mod.rs` | Re-export panel helpers |
| `crates/anie-tui/src/onboarding.rs` | Remove local copies; import from `widgets` |
| `crates/anie-tui/src/providers.rs` | Remove local copies; import from `widgets` |

### Sub-step A — Merge the implementations

Diff the two `centered_rect` implementations. Pick one. If they
disagree, pick whichever produces the behavior currently visible in
onboarding (that's the UX users have seen most) and document the
choice in a comment.

Same for `render_status_panel` / `render_busy_panel`. If the two
overlays diverge in color, icon, or footer, merge into one function
with parameters (e.g., `status_kind: StatusKind`).

### Sub-step B — Visual regression check

Run the onboarding flow end-to-end manually and confirm no visual
drift. Then run the provider-management overlay end-to-end and
confirm the same. This is the only step that requires hands-on
testing. Note what you tested in the PR description.

### Test plan

| # | Test |
|---|------|
| 1 | `centered_rect_returns_expected_area` for known inputs |
| 2 | `footer_line_applies_footer_style` |
| 3 | `previous_boundary_respects_grapheme` / `next_boundary_respects_grapheme` |
| 4 | Manual: onboarding status overlay renders identically to pre-refactor. |
| 5 | Manual: providers-management status overlay renders identically. |

### Exit criteria

- [ ] No duplicated helper in either overlay file.
- [ ] Both overlays compile and render unchanged.
- [ ] Unit tests for the pure functions pass.
- [ ] Manual visual check signed off.

---

## Phase 3 — Introduce the `OverlayScreen` trait

**Goal:** Replace `enum OverlayState` in `app.rs` with
`Option<Box<dyn OverlayScreen>>`. No overlay logic moves yet; this
phase only adds the trait and wires up dynamic dispatch.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/overlay.rs` | New — `trait OverlayScreen` and `enum OverlayAction` |
| `crates/anie-tui/src/lib.rs` | Add `pub mod overlay;` (or keep crate-private) |
| `crates/anie-tui/src/app.rs` | Replace `enum OverlayState` with `Option<Box<dyn OverlayScreen>>`; update the render and dispatch match arms |
| `crates/anie-tui/src/onboarding.rs` | Implement `OverlayScreen` for `OnboardingScreen` |
| `crates/anie-tui/src/providers.rs` | Implement `OverlayScreen` for `ProviderManagementScreen` |

### Sub-step A — Define the trait

Based on what `App` currently dispatches to each overlay, the trait
signature is approximately:

```rust
pub trait OverlayScreen: Send {
    fn handle_key(&mut self, key: KeyEvent, ctx: &mut OverlayContext<'_>) -> OverlayOutcome;
    fn handle_tick(&mut self, ctx: &mut OverlayContext<'_>) -> OverlayOutcome;
    fn handle_worker_event(&mut self, event: WorkerEvent, ctx: &mut OverlayContext<'_>) -> OverlayOutcome;
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect);
}
```

`OverlayContext` carries whatever `App` needs to pass in (theme,
async worker sender, credential store handle). Shape it to match
current call sites — do not invent new capabilities.

`OverlayOutcome` replaces whatever `OnboardingAction` /
`ProviderManagementAction` dispatches return. If the two existing
action types have common variants, unify; if they're disjoint, keep
a wrapper enum.

### Sub-step B — Migrate the two concrete overlays

Each existing screen already has `handle_key`, `handle_tick`,
`handle_worker_event`, and `render` methods. Wrap them in the trait
impl. Translate the existing action returns into `OverlayOutcome`.

### Sub-step C — Migrate `App`

Replace `self.overlay: OverlayState` with
`self.overlay: Option<Box<dyn OverlayScreen>>`. Replace the dispatch
match arms with single trait-method calls. Map `OverlayOutcome` back
into the `App`'s existing action handling.

### Test plan

| # | Test |
|---|------|
| 1 | `cargo check -p anie-tui` passes. |
| 2 | Existing `crates/anie-tui/src/tests.rs` suite passes unchanged. |
| 3 | New: `app_opens_and_closes_onboarding_overlay` |
| 4 | New: `app_opens_and_closes_provider_management_overlay` |
| 5 | Manual: exercise both overlays end-to-end; confirm no behavior drift. |

### Files that must NOT change

- `crates/anie-tui/src/model_picker.rs` — consumed by overlays, not
  an overlay itself.
- `crates/anie-cli/src/controller.rs` — the controller's
  `UiAction`/`OnboardingAction` interface is untouched; the action
  enum still reaches the controller.

### Exit criteria

- [ ] `enum OverlayState` is gone from `app.rs`.
- [ ] Both overlays implement `OverlayScreen`.
- [ ] `App` dispatches via the trait.
- [ ] All existing tests still pass; new trait-level tests pass.

---

## Phase 4 — Audit clone-heavy state and strong-type provider IDs

**Goal:** Remove the `Clone` calls flagged in the review and replace
`HashMap<String, _>` in `providers.rs` with a typed key.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/onboarding.rs` | Remove `.clone()` on `OnboardingState` at tick/render (lines 233, 284); rewrite to borrow `&self.state` |
| `crates/anie-tui/src/app.rs` | Remove `.clone()` on `Model` in decision trees where borrows work (lines 366, 690–691, 716, 728, 732) |
| `crates/anie-tui/src/providers.rs` | Replace `test_results: HashMap<String, TestResult>` with index-keyed `HashMap<usize, TestResult>` OR `HashMap<ProviderId, TestResult>` (newtype) |

### Sub-step A — `OnboardingState` clone removal

Each site that does `let state = self.state.clone(); match state {
... }` should become `match &self.state { ... }` — using pattern
borrow. For variants that require mutation (e.g., `mode.submit()`),
split into a borrow-then-replace pattern:

```rust
let next_state = match std::mem::replace(&mut self.state, OnboardingState::Transient) {
    OnboardingState::X(x) => handle_x(x),
    ...
};
self.state = next_state;
```

This uses a sentinel `Transient` variant OR `std::mem::take` if
`Default` is available, and avoids both the clone and the borrow
fight.

### Sub-step B — `Model` clone audit in `app.rs`

For each flagged clone, check if the downstream use only reads. If
so, refactor to borrow. Where the model must be owned (e.g., passed
to an async task), clone once at the call site.

### Sub-step C — Typed provider key

Option 1 (simpler): switch to `HashMap<usize, TestResult>` keyed by
row index. Test result is a per-row UI state; row index is its
natural key. Reset when the list is reloaded.

Option 2 (more general): define
`pub struct ProviderId(pub String);` in the provider module and use
`HashMap<ProviderId, TestResult>`. Typo-resistant at call sites;
slightly more code.

Pick Option 1 unless there is a cross-screen need for the provider
identifier.

### Test plan

| # | Test |
|---|------|
| 1 | `cargo clippy --workspace --all-targets -- -D warnings` passes (redundant_clone is denied, so this phase's success is partly signal-driven) |
| 2 | Manual: exercise onboarding state transitions; confirm no regressions |
| 3 | Manual: test a provider, delete it, test another; confirm test results track correctly |

### Exit criteria

- [ ] No `.clone()` on `OnboardingState`, `ProviderManagementMode`,
      or `Model` in tick/render hot paths.
- [ ] `test_results` uses a typed or indexed key.
- [ ] Clippy still clean.

---

## Phase 5 — Add overlay tests

**Goal:** The two overlays currently have zero tests. After phase 3
they are trait objects; add smoke tests that verify state machines
respond to key events.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/onboarding.rs` | Add `#[cfg(test)] mod tests` module |
| `crates/anie-tui/src/providers.rs` | Add `#[cfg(test)] mod tests` module |
| `crates/anie-tui/src/tests.rs` | *(optional)* add integration-level overlay tests if trait-level coverage is insufficient |

### Sub-step A — OnboardingScreen tests

| # | Test |
|---|------|
| 1 | `main_menu_down_arrow_moves_selection` |
| 2 | `main_menu_enter_picks_local_server_flow` |
| 3 | `local_server_waiting_to_detected_transition` (inject a `WorkerEvent`) |
| 4 | `provider_preset_list_enter_opens_api_key_form` |
| 5 | `custom_endpoint_form_fields_accept_input_in_order` |
| 6 | `discovery_error_transitions_to_status_screen` |
| 7 | `escape_returns_to_main_menu_from_any_flow` |

### Sub-step B — ProviderManagementScreen tests

| # | Test |
|---|------|
| 1 | `table_down_arrow_moves_selection` |
| 2 | `enter_opens_action_menu_for_selected_provider` |
| 3 | `test_provider_transitions_to_busy_then_status` |
| 4 | `delete_provider_removes_row` |
| 5 | `edit_api_key_stores_new_key_via_credential_store_mock` |
| 6 | `pick_model_opens_model_picker` |

These tests can use mock `CredentialStore` and mock worker-event
senders.

### Exit criteria

- [ ] At least 7 `OnboardingScreen` tests and 6
      `ProviderManagementScreen` tests pass.
- [ ] Tests use the public `OverlayScreen` trait where possible.
- [ ] Coverage for main state transitions is explicit.

---

## Phase 6 — Establish the `overlays/` directory

**Goal:** Move the two existing overlays into a dedicated
`overlays/` module and add placeholder files for the pi-shaped
overlays anie will eventually need. This is structural
scaffolding, not feature work — each placeholder is a short stub
with a `TODO` and no rendering.

pi-mono has these overlays today (see
`~/Projects/agents/pi/packages/coding-agent/src/modes/interactive/
components/`):

- `model-selector` (already exists in anie as `model_picker.rs`)
- `session-selector` + `session-selector-search`
- `settings-selector`
- `config-selector`
- `oauth-selector`
- `login-dialog`
- `theme-selector`
- `thinking-selector`
- `scoped-models-selector`
- `tree-selector`
- `extension-selector` / `extension-editor` / `extension-input`
- `user-message-selector`
- `show-images-selector`
- `hotkeys` (via command)

Not every one needs a placeholder. This phase adds placeholders
only for overlays that are on anie's near-term roadmap
(`docs/ideas.md`).

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/overlays/mod.rs` | New — module root; re-exports each submodule |
| `crates/anie-tui/src/overlays/onboarding.rs` | Move from `src/onboarding.rs` |
| `crates/anie-tui/src/overlays/providers.rs` | Move from `src/providers.rs` |
| `crates/anie-tui/src/overlays/model_picker.rs` | Move from `src/model_picker.rs` |
| `crates/anie-tui/src/lib.rs` | Replace `mod onboarding; mod providers; mod model_picker;` with `mod overlays;` and re-export top-level types through `overlays` |

(5 files, within cap. Placeholder stubs land in a follow-up sub-phase.)

### Sub-step A — Move the three existing overlays

1. `git mv crates/anie-tui/src/onboarding.rs crates/anie-tui/src/overlays/onboarding.rs`
2. `git mv crates/anie-tui/src/providers.rs crates/anie-tui/src/overlays/providers.rs`
3. `git mv crates/anie-tui/src/model_picker.rs crates/anie-tui/src/overlays/model_picker.rs`
4. Create `crates/anie-tui/src/overlays/mod.rs`:

```rust
//! Full-screen overlay screens.
//!
//! Each overlay implements the `OverlayScreen` trait (see
//! `super::overlay`) and is selected at runtime. The app holds
//! `Option<Box<dyn OverlayScreen>>`, not an enum, so adding a
//! new overlay is a matter of creating a file here and
//! implementing the trait.

mod onboarding;
mod providers;
mod model_picker;

pub use onboarding::{OnboardingScreen, OnboardingAction, OnboardingCompletion,
                     ConfiguredProvider, ConfiguredProviderKind};
pub use providers::{ProviderManagementScreen, ProviderManagementAction,
                    ProviderEntry, ProviderType, TestResult};
pub use model_picker::ModelPickerPane;
```

Adjust the exact re-export list to match the current public surface.

5. Update `crates/anie-tui/src/lib.rs` to reference the new module
   path. All external callers (`anie-cli`, tests) should continue
   to work unchanged because the public re-exports are preserved.

### Sub-step B — Add placeholder files for near-term overlays

Create empty stubs for the overlays most likely to land next,
based on `docs/ideas.md`. Each stub is a single file with:

- A doc comment describing what the overlay will be.
- A `pub struct <Name>;` placeholder with no fields.
- A `todo!()` or trivial `impl OverlayScreen` that renders
  "not yet implemented" and dismisses on any key.

This lets plan 02's trait catch all future callers; it does **not**
commit to implementing these overlays.

| File | Corresponds to pi |
|------|-------------------|
| `crates/anie-tui/src/overlays/session_picker.rs` | `session-selector.ts` — ties to `/resume`, `/session list` |
| `crates/anie-tui/src/overlays/settings.rs` | `settings-selector.ts` — ties to `/settings` from `docs/ideas.md` |
| `crates/anie-tui/src/overlays/oauth.rs` | `oauth-selector.ts` + `login-dialog.ts` — ties to `/login` from `docs/ideas.md` |
| `crates/anie-tui/src/overlays/theme_picker.rs` | `theme-selector.ts` — ties to theming from `docs/ideas.md` |
| `crates/anie-tui/src/overlays/hotkeys.rs` | `/hotkeys` builtin — ties to `/hotkeys` from `docs/ideas.md` |
| `crates/anie-tui/src/overlays/tree.rs` | `tree-selector.ts` — ties to `/tree` / session tree navigation |

Each placeholder file follows this template:

```rust
//! <Name> overlay.
//!
//! Corresponds to pi's `<pi-file>.ts`. Tracks with `docs/ideas.md`
//! item "<item name>". Not yet implemented — renders a
//! placeholder and dismisses on any key.

use crate::overlay::{OverlayContext, OverlayOutcome, OverlayScreen};
use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};

pub struct <Name>Screen;

impl <Name>Screen {
    pub fn new() -> Self {
        Self
    }
}

impl OverlayScreen for <Name>Screen {
    fn handle_key(&mut self, _key: KeyEvent, _ctx: &mut OverlayContext<'_>) -> OverlayOutcome {
        OverlayOutcome::Close
    }

    fn handle_tick(&mut self, _ctx: &mut OverlayContext<'_>) -> OverlayOutcome {
        OverlayOutcome::Idle
    }

    fn handle_worker_event(&mut self, _event: WorkerEvent, _ctx: &mut OverlayContext<'_>) -> OverlayOutcome {
        OverlayOutcome::Idle
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        // render "<Name> — not yet implemented. Press any key to close."
        // using crate::widgets::panel::render_status_panel (from phase 2)
    }
}
```

These stubs are intentionally rendered, compiled code — no
`todo!()` at runtime — so they don't panic if accidentally
surfaced. If a user somehow opens them, they see a clear message
and can close with any key.

### Sub-step C — Do NOT wire the stubs into slash commands

Adding stubs to the directory is structural. Wiring them into
`/settings`, `/login`, etc. is feature work that lands when each
overlay is actually implemented. Plan 02 does not register any
new slash commands.

### Sub-step D — Update `App` (if needed)

If `App` in `app.rs` imports the three moved overlays, update the
imports. If it uses `Option<Box<dyn OverlayScreen>>` from phase 3,
no further changes. If phase 3 has not yet landed when phase 6
runs, do phase 6 *after* phase 3.

### Files that must NOT change

- `crates/anie-tui/src/widgets/*` — shared widgets stay where
  phases 1–2 placed them.
- `crates/anie-tui/src/overlay.rs` — trait definition from phase 3
  is unchanged.
- Any call site that imports overlay types by their existing
  public names — the re-exports preserve them.

### Test plan

| # | Test |
|---|------|
| 1 | `cargo check -p anie-tui` passes |
| 2 | `cargo test -p anie-tui` passes (existing overlay tests find the overlays via the new path via re-exports) |
| 3 | `cargo clippy --workspace --all-targets -- -D warnings` passes |
| 4 | Manual: exercise onboarding end-to-end; visual unchanged |
| 5 | Manual: exercise provider management end-to-end; visual unchanged |
| 6 | `placeholder_overlay_compiles_and_renders_stub` — instantiate each placeholder, render into an off-screen buffer, assert the "not yet implemented" text appears |

### Exit criteria

- [ ] `crates/anie-tui/src/overlays/` exists as a module.
- [ ] `onboarding.rs`, `providers.rs`, `model_picker.rs` live
      under `overlays/` and their git history survives via
      `--follow`.
- [ ] Placeholder stubs exist for `session_picker`, `settings`,
      `oauth`, `theme_picker`, `hotkeys`, `tree` — each is a
      compilable `OverlayScreen` that renders "not implemented"
      and dismisses on any key.
- [ ] No external caller needed to change (public re-exports
      preserved).
- [ ] Adding a future overlay is: "create `overlays/foo.rs`,
      implement `OverlayScreen`, register with `/foo` command."
- [ ] Clippy clean, tests pass.

---

## Files that must NOT change in any phase

- `crates/anie-tui/src/input.rs` — main input editor.
- `crates/anie-tui/src/output.rs` — output pane.
- `crates/anie-tui/src/model_picker.rs` — picker widget (different
  concern).
- `crates/anie-tui/src/terminal.rs` — terminal init.
- `crates/anie-cli/src/*` — controller-side action types stay put.

## Dependency graph

```
Phase 1 (TextField) ──┐
Phase 2 (panel)     ──┼──► Phase 3 (trait) ──► Phase 4 (clone audit) ──► Phase 5 (tests)
                                                                             │
                                                                             ▼
                                                                      Phase 6 (overlays/ dir)
```

Phases 1 and 2 are independent and can ship in either order.
Phase 3 depends on both (so it imports from `widgets` cleanly).
Phase 4 is easier after 3 because the overlays are now objects.
Phase 5 is last before phase 6 because the test seams are
cleanest after 3 and 4. Phase 6 lands after 5 so its file moves
don't invalidate test paths mid-phase.

## Out of scope

- Adding a third overlay (e.g., `/settings`) — separate feature work.
- Redesigning the onboarding UX.
- Touching the main input editor.
- Theming — tracked in `docs/ideas.md`.
