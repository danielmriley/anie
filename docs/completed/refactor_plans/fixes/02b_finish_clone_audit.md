# Fix 02b — Finish the overlay clone audit + typed provider key

Closes out the two plan 02 phase 4 items that were deferred in the
original phase: the render-time `OnboardingState::clone()` and the
`HashMap<String, TestResult>` in provider management.

## Motivation

Plan 02 phase 4 exit criteria include:

> - No `.clone()` on `OnboardingState`, `ProviderManagementMode`,
>   or `Model` in tick/render hot paths.
> - `test_results` uses a typed or indexed key.

What's still out of compliance:

| Site | Status |
|---|---|
| `overlays/onboarding.rs:293` (`let state = self.state.clone(); match state { ... }` in `render`) | Clone every frame |
| `overlays/onboarding.rs:669, 677, 744, 749` (`return_to: Box::new(self.state.clone())` on transitions into pickers) | Clone on transition — not a hot path, but still driven by borrow-fight |
| `overlays/providers.rs:133, 181, 297` (`test_results: HashMap<String, TestResult>`) | String key |

The status note on plan 02 defended these as individually small,
but the cumulative effect is:

- Render-time clone of `OnboardingState` scales with state size.
  Each variant carries forms with `TextField`s holding owned
  `String`s; cloning once a frame is wasteful but more importantly
  it's a signal that ownership is fighting the code.
- The `HashMap<String, TestResult>` keyed on provider name has
  three call sites constructing the key and two call sites looking
  it up. A typo between `provider.name.clone()` (insertion) and
  `provider.name.to_string()` (lookup) would silently miss. This
  has almost certainly not caused a real bug, but it's exactly
  the kind of typo-prone surface Rust's type system can eliminate.

## Design principles

1. **Render never clones state.** The render path reads, it does
   not own. Pattern-match by reference.
2. **Transitions use take-and-replace, not clone.** When the
   overlay moves to a different state that needs to remember the
   previous one, use `std::mem::replace` + a transient sentinel
   variant, not `clone`.
3. **Typed keys over stringly-typed keys.** If the row's identity
   is its position in the list, key by `usize` and reset on list
   changes. If it's a logical provider identity, introduce a
   newtype — but prefer the simpler `usize`.

## Preconditions

Plan 02 phase 3 (overlay trait) and phase 5 (overlay tests) must
have landed. Both done on `refactor_branch`.

---

## Phase 1 — Render path: borrow, don't clone

**Goal:** `OnboardingScreen::render` pattern-matches
`&self.state`, no clone.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/overlays/onboarding.rs` | Replace `let state = self.state.clone(); match state { ... }` with `match &self.state { ... }`; adjust arm bodies to work with borrows |

### Sub-step A — Make render match-by-reference

Current (`overlays/onboarding.rs:293`):

```rust
let state = self.state.clone();
let spinner_frame = self.spinner.tick().to_string();
match state {
    OnboardingState::MainMenu { selected } => self.render_main_menu(frame, inner, selected),
    OnboardingState::ManagingProviders => { ... }
    ...
}
```

Target:

```rust
let spinner_frame = self.spinner.tick().to_string();
match &self.state {
    OnboardingState::MainMenu { selected } => self.render_main_menu(frame, inner, *selected),
    OnboardingState::ManagingProviders => { ... }
    ...
}
```

Each arm body changes from consuming the variant to borrowing it.
Variants whose fields are `Copy` (like `usize`, enum values) can be
dereffed cheaply. Variants carrying heavy payloads (forms,
`TextField`s, `Vec<ConfiguredProvider>`) pass `&Field` into the
renderer.

### Sub-step B — Adjust `render_*` helper signatures

Each `render_<variant>` helper that previously took owned fields
becomes `&`. Signature changes are confined to this file.

Audit all `self.render_<variant>(frame, area, ...)` call sites
and flip them from move to borrow. Prefer slice borrows
(`&[ConfiguredProvider]`) over `&Vec<...>`.

### Sub-step C — Confirm no mutation in the render path

Grep `fn render_` in `onboarding.rs`. None of these should mutate
`self.state`. If one does (e.g., advancing a spinner inside
render), move the mutation to `dispatch_tick` first.

### Test plan

| # | Test |
|---|------|
| 1 | Existing 21 onboarding tests still pass |
| 2 | New: `render_does_not_mutate_state` — construct screen, snapshot state, call render, confirm state equality with the snapshot (reuse `PartialEq` — add if missing, it's already `Clone`) |
| 3 | Clippy clean; `redundant_clone` is denied so this is enforced |
| 4 | Manual: smoke-test onboarding end-to-end; no visual change |

### Exit criteria

- [ ] `render` does not call `self.state.clone()`.
- [ ] Render-path helpers take borrows.
- [ ] All existing tests pass.

---

## Phase 2 — Transition path: take-replace over clone

**Goal:** `return_to: Box::new(self.state.clone())` becomes
`return_to: Box::new(std::mem::replace(&mut self.state, OnboardingState::Transient))`.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/overlays/onboarding.rs` | Add `OnboardingState::Transient` sentinel variant; migrate the four `return_to: Box::new(self.state.clone())` sites |

### Sub-step A — Add the sentinel variant

```rust
/// Sentinel variant used during take-and-replace transitions.
/// Never observed by render or tick — always replaced before the
/// next event-loop iteration. If the caller observes it, the
/// transition code failed to restore state.
Transient,
```

Add a `#[cfg(debug_assertions)]` `match self.state { Transient =>
panic!("Transient state leaked into dispatch") }` at the top of
both `dispatch_key` and `dispatch_tick` so a bug is noisy during
development and silent (rendered as "unknown state" or ignored) in
release. Alternative: handle `Transient` in render as "no content
until the next frame." Pick whichever feels least invasive —
start with the debug_assertions guard.

### Sub-step B — Migrate each `return_to` site

Current:

```rust
OnboardingAction::OpenModelPicker {
    return_to: Box::new(self.state.clone()),
    ...
}
```

Target:

```rust
OnboardingAction::OpenModelPicker {
    return_to: Box::new(std::mem::replace(&mut self.state, OnboardingState::Transient)),
    ...
}
```

The caller of `OnboardingAction::OpenModelPicker` handles the
return by setting `self.state = *return_to` when the picker
closes — already the case today. No caller change needed.

### Sub-step C — Preserve the exact behavior

Each transition previously produced a clone of the current state.
The replacement moves the value into the action and leaves
`Transient` behind. If the caller immediately restores the state
from `return_to`, the observable behavior is identical. If the
caller can delay restoration (e.g., picker stays open across a
tick or two), the screen is in `Transient` during that window —
which is why the render guard matters.

### Test plan

| # | Test |
|---|------|
| 1 | New: `open_model_picker_leaves_transient_state` — drive the overlay to a point where it opens the picker, assert `self.state` is `Transient` |
| 2 | New: `close_picker_restores_previous_state` — close the picker, assert `self.state` is the pre-picker variant |
| 3 | Existing onboarding tests still pass |
| 4 | Clippy clean (`redundant_clone` enforces cleanness here too) |

### Exit criteria

- [ ] Zero `.clone()` on `self.state` in `onboarding.rs`.
- [ ] `Transient` variant exists with a clear module-doc comment.
- [ ] Return-to transitions work end-to-end.

---

## Phase 3 — Strongly-type `test_results`

**Goal:** `ProviderManagementScreen::test_results` keyed by
`usize` (the row index), not `String`.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/overlays/providers.rs` | Change field type; update five call sites |

### Sub-step A — Signature change

```rust
// before
test_results: HashMap<String, TestResult>,

// after
test_results: HashMap<usize, TestResult>,
```

The three constructors zero it out to `HashMap::new()` — no
change there.

### Sub-step B — Migrate insertions

Find every `self.test_results.insert(name.clone(), ...)` and
replace with `self.test_results.insert(row_index, ...)`. The row
index is already available at every insertion site (it's the row
being tested).

### Sub-step C — Migrate lookups

Find every `self.test_results.get(&provider.name)` and replace
with `self.test_results.get(&row_index)`.

### Sub-step D — Reset on list reload

When the provider list is reloaded (triggered by
`ReloadConfig` or `ProviderManagementAction::ConfigChanged`), row
indices may shift. Clear `test_results` on reload:

```rust
self.test_results.clear();
```

Today's `HashMap<String, TestResult>` implicitly preserves results
across reloads by name. This behavior wasn't documented and isn't
obviously intentional — a reload invalidates row indices and the
results are per-row UI state anyway. Clearing is the right call.

If someone objects ("I want my test result to survive a reload"),
they can file an issue and we add a `HashMap<ProviderId, ...>` at
that point. Don't pre-optimize.

### Test plan

| # | Test |
|---|------|
| 1 | New: `test_result_keyed_by_row_index` — set a test result for row 2, assert lookup by index 2 returns it |
| 2 | New: `list_reload_clears_test_results` — seed a result, call the reload path, assert `test_results.is_empty()` |
| 3 | New: `deleting_a_row_does_not_leak_test_results` — add result for row 1, delete row 0, assert row 1's result is still findable (or explicitly cleared; pick one and lock it in the test) |
| 4 | Existing 11 provider-management tests still pass |
| 5 | Clippy clean |

### Exit criteria

- [ ] `test_results: HashMap<usize, TestResult>`.
- [ ] Reload clears `test_results`.
- [ ] No `String` key remains in `overlays/providers.rs`.

---

## Files that must NOT change

- `crates/anie-tui/src/overlays/model_picker.rs` — not part of this
  audit.
- `crates/anie-cli/*` — clone behavior in the controller is
  outside this plan.
- `crates/anie-tui/src/app.rs` `Model` clones called out in the
  original plan's Sub-step B — the status note says these were
  "largest items landed in the plan 00 followup (commit 107a840)";
  revisit only if clippy complains.

## Dependency graph

```
Phase 1 (render borrow)
Phase 2 (transition take-replace)
Phase 3 (typed test_results key)
```

All three are independent and can ship in either order. If
sequencing matters for PR size, do them in listed order — phase 1
is the highest-signal change; phase 3 is the smallest.

## Out of scope

- Broader clone audit in `anie-cli/src/controller.rs`. The
  controller still clones messages in places; reducing those is
  tracked in plan 08 phase F and later ideas.
- Introducing a `ProviderId` newtype. Only do this if a real
  multi-screen need for provider identity arises.
- Changing the `OnboardingAction` surface (i.e., the `return_to:
  Box<OnboardingState>` payload). The state machine stays as it
  is; only the source of the payload changes.
