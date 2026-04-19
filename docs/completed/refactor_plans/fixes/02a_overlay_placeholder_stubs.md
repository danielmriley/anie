# Fix 02a — Overlay placeholder stubs

Lands plan 02 phase 6 sub-step B, which was skipped when phase 6
originally shipped.

## Motivation

Plan 02 phase 6 had two sub-steps:

- **Sub-step A** — move the existing three overlays (`onboarding`,
  `providers`, `model_picker`) into `crates/anie-tui/src/overlays/`.
- **Sub-step B** — add compilable placeholder stubs for six pi-shaped
  overlays anie's roadmap calls for: `session_picker`, `settings`,
  `oauth`, `theme_picker`, `hotkeys`, `tree`.

Sub-step A landed. Sub-step B didn't. `overlays/mod.rs:12` even
documents the opposite policy ("land here as they're implemented"),
which directly contradicts the phase's rationale.

Re-reading plan 02 on why the stubs matter:

> Directory scaffolding is cheap and prevents re-migrating overlays
> as they're added.

and:

> These stubs are intentionally rendered, compiled code — no
> `todo!()` at runtime — so they don't panic if accidentally
> surfaced. If a user somehow opens them, they see a clear message
> and can close with any key.

The concrete payoff: the first feature PR that adds one of these
overlays (e.g., `/settings` from `docs/ideas.md`) then has a clear
landing pad. Today it does not.

## Design principles

1. **Compile-and-render, not `todo!()`.** Each stub renders a
   dismissable "not yet implemented" message.
2. **Match the existing `OverlayScreen` shape exactly.** No new
   trait variants; no new `OverlayOutcome` cases unless the stub
   specifically needs to close itself (it does — see Phase 2).
3. **Zero dispatch wiring.** These stubs do NOT get slash commands
   wired to them. Adding the slash command is feature work, not
   scaffolding.
4. **One file per stub.** Small and identical structure so adding
   real implementations is a find-and-replace.

## Preconditions

Plan 02 phases 1–5 and phase 6 sub-step A must have landed. All
confirmed done on `refactor_branch`.

## Current state reference

```
crates/anie-tui/src/overlays/
├── mod.rs
├── model_picker.rs      ← real
├── onboarding.rs        ← real
└── providers.rs         ← real
```

Target:

```
crates/anie-tui/src/overlays/
├── mod.rs
├── model_picker.rs
├── onboarding.rs
├── providers.rs
├── session_picker.rs    ← stub
├── settings.rs          ← stub
├── oauth.rs             ← stub
├── theme_picker.rs      ← stub
├── hotkeys.rs           ← stub
└── tree.rs              ← stub
```

---

## Phase 1 — Add the stub template + a helper in `widgets::panel`

**Goal:** Every stub overlay renders the same
"not-yet-implemented" card via a shared helper. Making that helper
is phase 1 so phase 2 is pure boilerplate.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/widgets/panel.rs` | Add `render_placeholder_panel(frame, area, title, body)` that renders a centered bordered paragraph with a "Press any key to close" footer |
| `crates/anie-tui/src/widgets/mod.rs` | Re-export `render_placeholder_panel` |

### Sub-step A — Helper signature

```rust
pub(crate) fn render_placeholder_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    body: &str,
) {
    // Use existing `centered_rect` for sizing.
    // Block with border + `title`.
    // Paragraph with `body` + blank line + "Press any key to close."
    // Footer style = panel footer style (reuse existing helper).
}
```

### Sub-step B — Unit test for the helper

```rust
#[test]
fn placeholder_panel_renders_body_and_footer() {
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| {
        render_placeholder_panel(
            frame,
            frame.area(),
            "Settings",
            "Configuration screen not yet implemented.",
        );
    }).unwrap();
    let buffer = terminal.backend().buffer();
    let rendered = buffer_to_string(buffer);
    assert!(rendered.contains("Settings"));
    assert!(rendered.contains("not yet implemented"));
    assert!(rendered.contains("Press any key to close"));
}
```

Use whatever `buffer_to_string`-equivalent the existing widget
tests use; don't invent a new one.

### Exit criteria

- [ ] `render_placeholder_panel` exists and is covered by a unit
      test.
- [ ] Clippy clean.

---

## Phase 2 — Land the six stub overlays

**Goal:** Six new files, each a minimal `OverlayScreen` impl.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/overlays/session_picker.rs` | New — stub |
| `crates/anie-tui/src/overlays/settings.rs` | New — stub |
| `crates/anie-tui/src/overlays/oauth.rs` | New — stub |
| `crates/anie-tui/src/overlays/theme_picker.rs` | New — stub |
| `crates/anie-tui/src/overlays/hotkeys.rs` | New — stub |
| `crates/anie-tui/src/overlays/tree.rs` | New — stub |

(Six files, exceeding the 5-file cap. Split into two PRs: four
stubs in the first, two in the second — pick any partition. Each
file is ~40 LOC of identical scaffolding, so the cap exists more
for the reviewer than for complexity; a single PR with all six is
also acceptable if the reviewer agrees.)

### Sub-step A — Decide on dispatch shape for stubs

The current `OverlayScreen` trait returns `OverlayOutcome`, and
`OverlayOutcome` is a flat union of
`Onboarding(OnboardingAction)` and
`ProviderManagement(ProviderManagementAction)`. A stub has no
native Action enum.

Two options:

1. **Add `OverlayOutcome::Dismiss`** — a universal "close me"
   variant, added to the enum and handled by `App`. Stubs return
   `Dismiss` on any key.
2. **Each stub gets its own `Action` enum with a `Dismiss` variant.**
   Consistent with existing overlays but verbose.

**Pick option 1.** It's cleaner and it's the natural variant for
anything else that closes on-dismiss (future transient confirmation
overlays). `App::apply_overlay_outcome` grows a one-line match arm.

If `OverlayOutcome` carrying a universal `Dismiss` turns out to
conflict with how the real overlays close themselves, revisit —
but the existing close paths return overlay-specific action
variants that `App` interprets as "close," so a shared `Dismiss`
does not change their behavior.

### Sub-step B — Stub template

Every stub file is:

```rust
//! <Name> overlay — placeholder.
//!
//! Corresponds to pi-mono's `<pi-file>.ts`. Tracks with
//! `docs/ideas.md` item "<name>". Not yet implemented — renders
//! a stub and dismisses on any key.
//!
//! Wire-up is intentionally absent: the slash command / keybinding
//! that opens this overlay lands with the real implementation,
//! not here.

use crossterm::event::KeyEvent;
use ratatui::{Frame, layout::Rect};

use crate::overlay::{OverlayOutcome, OverlayScreen};
use crate::widgets::render_placeholder_panel;

/// Placeholder <name> screen.
pub(crate) struct <Name>Screen;

impl <Name>Screen {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl OverlayScreen for <Name>Screen {
    fn dispatch_key(&mut self, _key: KeyEvent) -> OverlayOutcome {
        OverlayOutcome::Dismiss
    }

    fn dispatch_tick(&mut self) -> OverlayOutcome {
        OverlayOutcome::Dismiss // any tick without user input is also a no-op close? NO — see Sub-step C
    }

    fn dispatch_render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        render_placeholder_panel(
            frame,
            area,
            "<Title>",
            "<Body>",
        );
    }
}
```

### Sub-step C — Tick behavior

`dispatch_tick` must NOT close the overlay — a tick happens
several times a second without user interaction. Ticks should
return a "still open, no state change" outcome.

Add a third `OverlayOutcome` variant if needed:

```rust
pub(crate) enum OverlayOutcome {
    Onboarding(OnboardingAction),
    ProviderManagement(ProviderManagementAction),
    Dismiss,         // new — close the overlay
    Idle,            // new — no action, stay open
}
```

`App::apply_overlay_outcome` handles `Idle` by doing nothing.

### Sub-step D — Per-stub title + body text

| Stub | Title | Body |
|---|---|---|
| `session_picker` | "Session Picker" | "Session selection UI not yet implemented. Use `/session list` and `/session <id>` for now." |
| `settings` | "Settings" | "Settings overlay not yet implemented. Edit `~/.anie/config.toml` directly for now." |
| `oauth` | "OAuth / Login" | "OAuth sign-in not yet implemented. Use `/onboard` to configure providers with API keys." |
| `theme_picker` | "Theme Picker" | "Theme selection not yet implemented." |
| `hotkeys` | "Keyboard Shortcuts" | "Hotkey reference not yet implemented. See the README for the current bindings." |
| `tree` | "Session Tree" | "Session tree navigation not yet implemented." |

Adjust wording as taste dictates but keep it short — one line of
context, one line of fallback guidance where applicable.

### Sub-step E — Register in `overlays/mod.rs`

Add module declarations. Do **not** re-export the stub types —
they have no external consumers yet, and adding re-exports invites
premature feature-wiring.

```rust
pub(crate) mod model_picker;
pub(crate) mod onboarding;
pub(crate) mod providers;

// Placeholder overlays — land as they're wired up (plan 02 phase 6 sub-step B).
pub(crate) mod session_picker;
pub(crate) mod settings;
pub(crate) mod oauth;
pub(crate) mod theme_picker;
pub(crate) mod hotkeys;
pub(crate) mod tree;
```

Update the `//! Full-screen overlay screens.` doc comment to drop
the "land here as they're implemented" sentence that contradicts
the plan.

### Test plan

| # | Test |
|---|------|
| 1 | `cargo check -p anie-tui` passes |
| 2 | `cargo test -p anie-tui` passes |
| 3 | One test per stub: `<name>_placeholder_renders_title_and_body` — construct the stub, draw into a test backend, assert the title + body appear |
| 4 | One shared test: `<name>_placeholder_dismisses_on_any_key` — call `dispatch_key` with several different `KeyEvent`s, assert each returns `OverlayOutcome::Dismiss` |
| 5 | One shared test: `<name>_placeholder_tick_keeps_open` — call `dispatch_tick`, assert it returns `Idle` |
| 6 | Clippy clean |

Six stubs × 3 tests = 18 new tests. Each test is ~15 LOC.

### Files that must NOT change

- `crates/anie-tui/src/app.rs` beyond adding the two
  `OverlayOutcome` arms (`Dismiss` → `self.overlay = None;`,
  `Idle` → no-op).
- `crates/anie-tui/src/overlays/onboarding.rs`,
  `providers.rs`, `model_picker.rs` — they keep their current
  outcomes.
- `crates/anie-cli/*` — no controller wiring.

### Exit criteria

- [ ] Six stub files exist and compile.
- [ ] Each stub has three tests (render + dismiss + tick-idle).
- [ ] `OverlayOutcome::Dismiss` + `Idle` added and handled in
      `App::apply_overlay_outcome`.
- [ ] `overlays/mod.rs`'s "land here as they're implemented"
      doc line is replaced with a pointer to this plan.
- [ ] No slash command or keybinding references a stub.

---

## Divergence from parent plan

Plan 02 phase 6 sub-step B described the stubs with a bare
`impl OverlayScreen` pattern that returned `OverlayOutcome::Close`
(a variant it assumed existed). We landed the trait with different
outcome shapes — `OverlayOutcome` is a flat union over concrete
overlay action enums, with no `Close`. This plan adds the
universal `Dismiss` + `Idle` variants rather than forcing each stub
to invent its own action type. The original plan's intent (a
dismissable stub) is preserved; the mechanism is updated to match
the actual trait.

## Files that must NOT change

- `crates/anie-cli/src/*` — slash commands that would open a stub
  are future feature work.
- `docs/arch/anie-rs_architecture.md` — this scaffolding is not an
  architecture change.
- `crates/anie-tui/src/overlay.rs` beyond the two new outcome
  variants.

## Dependency graph

```
Phase 1 (render_placeholder_panel) ──► Phase 2 (six stubs)
```

## Out of scope

- Wiring `/settings`, `/login`, `/hotkeys`, `/tree`,
  `/session-picker`, or theme switching to their stubs. That's
  feature work for the PR that actually implements each overlay.
- Designing the real UI for any of these overlays.
- Adding new slash commands to `UiAction`.
- Adding the same stubs to `commands.rs` metadata — the command
  registry currently only reflects `UiAction` variants, and these
  overlays don't have those yet.
