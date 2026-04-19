# Fix 03a — Slash-command dispatch (finish plan 03 phase 3)

Either wire the existing `CommandRegistry` into the actual slash
command flow, or formally narrow plan 03 phase 3 so the current
state is not a half-measure.

## Motivation

Plan 03 phase 3 promised a registry-backed dispatch. What landed is
metadata only:

- `crates/anie-cli/src/commands.rs` has a `SlashCommandInfo` struct,
  a `CommandRegistry`, and a `SlashCommandSource` enum.
- `ControllerState` carries a `command_registry: CommandRegistry`
  field, populated with `CommandRegistry::with_builtins()` at
  startup.
- Five of the registry's methods (`lookup`, `all`,
  `grouped_by_source`, `register`, `SlashCommandSource::label`) plus
  the entire `DuplicateCommand` error type are flagged with
  `#[allow(dead_code)] // consumed once /help lands` — they are not
  used.
- `handle_action` at `controller.rs:408` is still a 20-arm flat
  `match action { UiAction::X => ... }` block, handling slash
  commands identically to the pre-plan state.

The exit criterion "handle_action contains no slash-command match
arms" is unmet. The exit criterion "Adding a new `/settings` or
`/copy` command is: write a `SlashCommand` impl, register in
`CommandRegistry::with_builtins()`" is unmet — adding a new command
today still requires adding a `UiAction` variant and a
`handle_action` arm.

Two routes forward:

- **Route A (recommended):** wire the registry into `/help`, close
  the dead-code gap, and accept that dispatch stays with the
  `UiAction` match for built-ins. This matches what `commands.rs`
  already claims to be doing ("pi-style metadata separate from
  dispatch") and is achievable in a day.
- **Route B:** build the full `SlashCommand` trait + dispatch that
  plan 03 phase 3 originally specified. This is a larger refactor,
  roughly two to three days, touches `handle_action` substantially,
  and probably benefits from being paired with plan 10 phase 4
  (extension-registered commands).

This plan describes **Route A** as the primary path, with Route B
documented as an addendum for the future if plan 10 phase 4 pulls
us that way.

## Design principles

1. **If it's in the codebase, it's used.** `#[allow(dead_code)]`
   comments saying "will be used when X lands" are a flag that X
   should land or the code should come out.
2. **Match the claim.** `commands.rs`'s module doc claims "dispatch
   is NOT owned by this module" and says it's pure metadata. Make
   `/help` actually consume that metadata. Today the claim is
   aspirational.
3. **Don't pre-build for extensions.** The `SlashCommandSource`
   variants for `Extension`, `Prompt`, `Skill` stay in the enum,
   but nothing in this plan constructs them. Plan 10 phase 4 will.

## Preconditions

- Plan 03 phases 1, 2, 4, 5 all landed.
- `commands.rs` registry exists with `with_builtins()`.

---

## Phase 1 — Wire `/help` to consume the registry

**Goal:** The `/help` output is derived from
`CommandRegistry::all()`, grouped by `SourceKey`. Adding a
builtin to `builtin_commands()` surfaces it in `/help`
automatically.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/commands.rs` | Add `format_help(&CommandRegistry) -> String` that groups commands by `SourceKey` and returns the rendered help text; delete `#[allow(dead_code)]` on `lookup`, `all`, `grouped_by_source`, `SlashCommandSource::label`, `SlashCommandInfo::summary` |
| `crates/anie-tui/src/app.rs` | Add `UiAction::ShowHelp` (or verify existing surfacing); ensure Help is produced when user types `/help` |
| `crates/anie-cli/src/controller.rs` | In `handle_action`, on `UiAction::ShowHelp`, build the formatted help via `self.state.command_registry` and emit it as a `SystemMessage` |

### Sub-step A — Decide the exact `/help` format

Match pi's shape:

```
Commands:
  Builtin:
    /model         Select model (opens picker on no args)
    /thinking      Set reasoning effort: off|low|medium|high
    ...
```

If a group is empty (e.g., no extensions yet), drop that group
header entirely. Today only `Builtin` will appear.

### Sub-step B — Add `format_help`

```rust
impl CommandRegistry {
    /// Render `/help` output, grouped by source.
    pub(crate) fn format_help(&self) -> String {
        let mut out = String::from("Commands:\n");
        for (key, entries) in self.grouped_by_source() {
            out.push_str("  ");
            out.push_str(group_heading(key));
            out.push_str(":\n");
            for info in entries {
                out.push_str(&format!("    /{:12} {}\n", info.name, info.summary));
            }
        }
        out
    }
}

fn group_heading(key: SourceKey) -> &'static str {
    match key {
        SourceKey::Builtin => "Builtin",
        SourceKey::Extension => "Extensions",
        SourceKey::Prompt => "Prompts",
        SourceKey::Skill => "Skills",
    }
}
```

This is the only new behavior code. Everything else is wiring.

### Sub-step C — Ensure `/help` flows into a `UiAction::ShowHelp`

Check how `/help` is currently handled:

- If `/help` is parsed as a distinct `UiAction` today, great — just
  rewrite that arm of `handle_action` to call
  `self.state.command_registry.format_help()`.
- If `/help` currently resolves to some other variant (e.g., a
  `UiAction::SubmitPrompt("/help")` that the server side
  interprets), add a new `UiAction::ShowHelp` variant to
  `crates/anie-tui/src/app.rs` and route `/help` input to it from
  the input parser.

Grep `handle_action` for any existing help-like arm — adopt it if
present.

### Sub-step D — Delete the dead-code markers

After `format_help` is used, remove:

- `#[allow(dead_code)]` on `CommandRegistry::lookup` — keep only if
  still unused
- `#[allow(dead_code)]` on `CommandRegistry::all`
- `#[allow(dead_code)]` on `CommandRegistry::grouped_by_source`
- `#[allow(dead_code)]` on `CommandRegistry::register`
- `#[allow(dead_code)]` on `SlashCommandSource::label`
- `#[allow(dead_code)]` on `SlashCommandInfo::summary` (field,
  accessed by `format_help`)
- `#[allow(dead_code)]` on `SourceKey`
- `#[allow(dead_code)]` on `DuplicateCommand`

If any of these is still unused after phase 1, it belongs in a
later phase or should be deleted. The default is delete. Keep
`register` / `DuplicateCommand` since plan 10 phase 4 has a
concrete consumer in mind, but prefer `#[cfg(test)] pub(crate) fn
test_register_extension(...)` or similar to narrow the surface.

### Test plan

| # | Test |
|---|------|
| 1 | `format_help_starts_with_commands_heading` |
| 2 | `format_help_includes_every_builtin_name` |
| 3 | `format_help_renders_extensions_section_when_registered` — register a mock extension entry, assert output contains `"Extensions"` and the entry |
| 4 | `format_help_omits_empty_sections` — with only builtins, output contains no `"Extensions"` heading |
| 5 | New controller-level test: `help_command_emits_system_message_with_registry_output` (mocks the event channel, sends `UiAction::ShowHelp`, asserts the emitted `SystemMessage` text equals `format_help(...)`) |
| 6 | Existing controller tests pass |
| 7 | Manual: run `anie`, type `/help`, verify the output matches |

### Exit criteria

- [ ] `/help` output is derived from the registry.
- [ ] No `#[allow(dead_code)]` on `CommandRegistry::all`,
      `grouped_by_source`, `format_help`, or `summary`.
- [ ] Adding a new builtin to `builtin_commands()` surfaces it in
      `/help` without touching any other file.

---

## Phase 2 — Consistency audit: registry ↔ UiAction

**Goal:** `builtin_commands()` and the `handle_action` match arms
agree. Adding a command means updating both, and a CI-level check
(or a test) catches drift.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/commands.rs` | Add `command_name_for_action(action: &UiAction) -> Option<&'static str>` OR a const `BUILTIN_COMMAND_NAMES: &[&str]` and assert at startup that every `UiAction` slash variant has a corresponding entry |
| `crates/anie-cli/src/controller.rs` | On `ControllerState::new`, assert registry contains an entry for every dispatched slash command. Alternatively, implement this as a `#[cfg(test)]` test |

### Sub-step A — Choose consistency mechanism

Two options:

1. **Runtime assertion at startup.** Panic-loud if the registry
   and the dispatch diverge. Easy, but panics in prod are ugly.
2. **Unit test.** Enumerate every `UiAction` variant that maps to
   a slash command; assert `registry.lookup(name).is_some()` for
   each.

**Pick option 2.** A `#[test]` in `commands.rs` referencing each
known slash-command name:

```rust
#[test]
fn registry_covers_every_dispatched_slash_command() {
    let dispatched = ["model", "thinking", "compact", "fork", "diff",
                      "new", "session", "tools", "onboard", "providers",
                      "clear", "reload", "copy", "help", "quit"];
    let registry = CommandRegistry::with_builtins();
    for name in dispatched {
        assert!(
            registry.lookup(name).is_some(),
            "registry missing builtin '{name}' — \
             update builtin_commands() when adding a UiAction slash variant"
        );
    }
}
```

Maintain the `dispatched` list manually — it mirrors the
`handle_action` arms. This is weaker than a compiler guarantee but
catches the realistic failure mode ("I added `/foo` but forgot to
register it").

### Sub-step B — Comment on the coupling

At the top of `builtin_commands()` in `commands.rs`, update the
existing comment to say:

```rust
/// The builtin anie slash-command catalog.
///
/// Keep this in sync with the `UiAction` dispatch in
/// `controller::InteractiveController::handle_action`. The test
/// `registry_covers_every_dispatched_slash_command` enforces the
/// coupling.
///
/// Adding a builtin:
///   1. Add a `UiAction` variant in `anie-tui::app`.
///   2. Parse `/name` into that variant in `anie-tui::input`.
///   3. Handle it in `handle_action`.
///   4. Add a `SlashCommandInfo::builtin(...)` here.
///   5. Add the name to the `dispatched` list in the test.
```

### Exit criteria

- [ ] The coverage test passes.
- [ ] The comment reflects the actual five-step add-a-builtin
      recipe.

---

## Phase 3 — Retire unused registry surface (optional)

**Goal:** Anything still unused after phases 1–2 is deleted.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/commands.rs` | Remove or narrow methods that have no consumer after phase 1 |

### Sub-step A — Audit

After phase 1 lands, grep each public registry method:

- `lookup` — used by the coverage test (phase 2) and future
  autocomplete. Keep.
- `all` — used by `format_help` (phase 1). Keep.
- `grouped_by_source` — used by `format_help`. Keep.
- `register` — no consumer until plan 10 phase 4. Options:
  - Keep `pub(crate)` with a comment "consumed by plan 10 phase 4."
  - Narrow to `#[cfg(test)] pub(crate) fn register(...)` so the
    existing tests still compile.
  - Delete and re-add in plan 10.

**Pick narrow to `#[cfg(test)]`.** Reason: the tests assert the
duplicate policy today. Deleting the method drops those tests —
fine, but they're cheap and document the intent. Narrowing
keeps the tests while removing the "this is a public API" signal.

- `DuplicateCommand` — same treatment as `register`: gate on
  `#[cfg(test)]`.

### Sub-step B — Preserve extension variants

Do **NOT** remove `SlashCommandSource::{Extension, Prompt, Skill}`.
They're:

- Part of the wire contract plan 10 consumes.
- Demonstrated by the tests added in plan 03 phase 3.

Leaving them as enum variants is cheap; constructing them is
future work.

### Test plan

| # | Test |
|---|------|
| 1 | Existing `commands::tests` still pass (the tests that call `register` get gated with `#[cfg(test)]` methods) |
| 2 | Clippy clean; no remaining `#[allow(dead_code)]` in `commands.rs` |

### Exit criteria

- [ ] Zero `#[allow(dead_code)]` in `commands.rs`.
- [ ] Every public item has a real consumer or is `#[cfg(test)]`.
- [ ] Extension-related enum variants remain intact.

---

## Divergence from parent plan

Plan 03 phase 3 specified a `trait SlashCommand` with `async fn
dispatch(...)` and handler impls in `commands/builtin.rs`. We are
not building that. Reasons:

- The `UiAction` match in `handle_action` does one thing well:
  enum-variant dispatch into code with direct access to
  `ControllerState`. A trait-object dispatch would need to hand a
  `&mut ControllerState` through a trait method, which — given
  today's nested borrows into `self.state.session`, `self.state.
  compaction`, `self.state.command_registry` — would likely need
  a thin handler struct per command anyway.
- Pi-mono keeps the same split: metadata in `slash-commands.ts`,
  dispatch inline. The revision note on plan 03 phase 3 already
  acknowledges this: "pi's own slash-commands.ts also keeps
  dispatch separate from metadata."
- Plan 10 phase 4 (extension-registered commands) goes through an
  IPC boundary that is not a Rust trait method. The registry's
  `SlashCommandSource::Extension` variant and its `label()` are
  the useful pieces for that — not a `SlashCommand` trait.

If plan 10 ends up wanting a Rust trait, revisit. For now, the
narrower shape is correct and the plan 03 phase 3 exit criteria are
restated as:

- ~~"handle_action contains no slash-command match arms"~~ →
  "`/help` output is derived from the registry and adding a
  builtin doesn't require any code outside `commands.rs` +
  the existing three `UiAction` touch points."
- ~~"Adding a new /settings or /copy command is: write a
  SlashCommand impl, register"~~ → "Adding a new `/foo` command
  is: `UiAction::Foo` variant, input-parser rule,
  `handle_action` arm, `SlashCommandInfo::builtin(...)` entry."

Plan 03 phase 3's status note should be updated to reflect this;
this fix plan does that update implicitly by superseding it.

---

## Files that must NOT change

- The `SlashCommandSource::{Extension, Prompt, Skill}` variants —
  plan 10 phase 4 needs them.
- `crates/anie-tui/src/input.rs` beyond the `/help` parsing rule
  (if absent).
- `crates/anie-agent/*`.

## Dependency graph

```
Phase 1 (wire /help) ──► Phase 2 (consistency) ──► Phase 3 (trim)
```

Phase 1 is the core; phases 2 and 3 lock in what phase 1
established. All three can land in a single day.

## Out of scope

- The full `SlashCommand` trait (Route B above). If we end up
  needing it, it lands with plan 10 phase 4 or a fix-plan
  successor.
- Adding `/help <command>` long-form usage. `SlashCommandInfo`
  already has `summary`; a `usage` field could come later if
  needed.
- Autocomplete — tracked in `docs/ideas.md`.
- Wiring a stub overlay (see fix 02a) to a slash command. That's
  feature work that lands with the real implementation.
