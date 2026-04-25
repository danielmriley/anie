# 05 — Simplifications

## Rationale

These items aren't perf bugs. They're places where the code is
harder to keep correct under load — which matters because PR
01–04 are touching the same neighborhoods. Smaller, simpler
surface area means less drift between intent and behavior.

Findings F-3, F-18, F-19, F-20.

## Items

### F-3: collapse `dispatch_validated_command`

`crates/anie-tui/src/app.rs:1100-1293` — 195 lines, ~15 arms,
most of which are a single-line `self.action_tx.send(UiAction::*)`
differing only in the variant. The structural pattern repeats
enough that adding a new command (`/state`, `/markdown`,
`/tool-output`, `/login`, `/logout` recently) is mostly
copy-paste with a one-character difference.

**Proposed shape:**

```rust
fn dispatch_validated_command(&mut self, info: &SlashCommandInfo, arg: Option<&str>) {
    let action = match info.name {
        // Commands with custom dispatch logic — keep these arms.
        "model" => return self.dispatch_model(arg),
        "thinking" => return self.dispatch_thinking(arg),
        "context-length" => UiAction::ContextLength(arg.map(str::to_string)),
        "session" => return self.dispatch_session(arg),
        "clear" => { self.output_pane.clear(); UiAction::ClearOutput },
        // Commands with no args, no side effects — table-driven.
        name => match noarg_action(name) {
            Some(action) => action,
            None => return,
        },
    };
    let _ = self.action_tx.send(action);
}

fn noarg_action(name: &str) -> Option<UiAction> {
    match name {
        "compact" => Some(UiAction::Compact),
        "fork" => Some(UiAction::ForkSession),
        "diff" => Some(UiAction::ShowDiff),
        "new" => Some(UiAction::NewSession),
        "tools" => Some(UiAction::ShowTools),
        "state" => Some(UiAction::ShowState),
        "help" => Some(UiAction::ShowHelp),
        "quit" => Some(UiAction::Quit),
        "copy" => Some(UiAction::CopyLastAssistant),
        "reload" => Some(UiAction::ReloadConfig { provider: None, model: None }),
        "onboard" => Some(UiAction::OpenOnboarding),
        "providers" => Some(UiAction::OpenProviders),
        _ => None,
    }
}
```

The complex commands (`model`, `thinking`, `session`) keep
custom arms. Everything else is a flat lookup. `dispatch_validated_command`
shrinks from 195 lines to ~30 plus a small lookup helper. New
no-arg commands become a one-line addition.

### F-18: drop unused `_is_streaming` parameter

`crates/anie-tui/src/output.rs:1183-1196`:
```rust
fn assistant_answer_lines(text: &str, width: u16, _is_streaming: bool, ctx: &RenderContext) -> Vec<Line<'static>> {
```

The parameter is unused. The branch logic that consumed it
moved into `StreamingAssistantRender`. Drop the parameter,
update call sites. Per CLAUDE.md "Avoid backwards-compatibility
hacks," this is a straight delete.

### F-19: merge scroll arms in idle/active key handlers

`crates/anie-tui/src/app.rs:911-997`. `handle_idle_key` and
`handle_active_key` both handle `PageUp`, `PageDown`, `Home`,
`End` identically — call the corresponding `output_pane.scroll_*`
and return `RenderDirty::full()`. The actual difference is
narrow:

- `handle_idle_key` accepts `Ctrl+L` (clear), `Ctrl+C` (quit).
- `handle_active_key` rejects `Ctrl+L` and treats `Ctrl+C` as
  abort.

**Proposed shape:**

```rust
fn handle_key(&mut self, key: KeyEvent) -> RenderDirty {
    if let Some(dirty) = self.handle_scroll_key(key) {
        return dirty;
    }
    match self.agent_state {
        AgentUiState::Idle => self.handle_idle_only_key(key),
        _ => self.handle_active_only_key(key),
    }
}

fn handle_scroll_key(&mut self, key: KeyEvent) -> Option<RenderDirty> {
    match key.code {
        KeyCode::PageUp => { self.output_pane.scroll_page_up(); Some(RenderDirty::full()) },
        KeyCode::PageDown => { self.output_pane.scroll_page_down(); Some(RenderDirty::full()) },
        KeyCode::Home => { ... },
        KeyCode::End => { ... },
        _ => None,
    }
}
```

Net change: ~30 lines collapsed; scroll behavior is now
defined in one place; future scroll-binding changes can't drift
between the two handlers.

### F-20: tighten `RenderDirty` state space

`crates/anie-tui/src/app.rs:118-164`. The struct allows
`(composer=true, transcript=true)` but only three of the four
combinations are produced in practice. The `merge` method is
correct but defensive against state combinations that don't
arise.

**Lightest fix:** keep the struct but add a doc-comment listing
the actual three states and a debug-assert in `merge` that
flags the unused combination if it ever arises. That preserves
the option without paying the cost.

**Heavier fix:** replace with an enum:
```rust
enum RenderDirty {
    None,
    ComposerOnly,
    Full,
}
```
with `merge` defined as `max`. Reviewers may prefer this; it's
~10 lines of code change.

The reviewer's preference probably wins here since it's a
readability call.

## Files to touch

- `crates/anie-tui/src/app.rs` — F-3, F-19, F-20.
- `crates/anie-tui/src/output.rs` — F-18.
- Tests stay where they are.

## Phased PRs

This plan is one bundled cleanup PR. Each item is small; the
file diffs are localized.

If reviewers prefer split:
- PR 05a: F-18 (drop dead parameter) — trivial, can land
  alongside any of PR 01–04.
- PR 05b: F-3 (dispatch table) — biggest diff, separate review.
- PR 05c: F-19 + F-20 (key dispatch + RenderDirty) — together
  because they touch the same handler shape.

## Test plan

1. **All existing TUI tests must pass unchanged.** This is
   refactor, not behavior change. If a test breaks, the change
   broke something.
2. **`dispatch_validated_command_dispatches_every_builtin`** —
   add a coverage test that walks every name in the
   `CommandRegistry`, drives `dispatch_validated_command` with
   it, asserts a `UiAction` was sent. Guards against forgetting
   to add a new command to the table.
3. **`scroll_keys_behave_identically_in_idle_and_active`** —
   property test on the four scroll keys; assert the same
   `RenderDirty` and the same scroll delta in both
   `AgentUiState::Idle` and `AgentUiState::Streaming`.

## Risks

- **Hidden behavior in the long match.** A 195-line match might
  have a one-line arm that does something subtly different
  from "send UiAction." Read every arm before collapsing.
- **`RenderDirty` enum migration.** If F-20 picks the enum
  path, every call site must update. Use the compiler:
  break-change everywhere first, fix one call site at a time.

## Exit criteria

- `dispatch_validated_command` is under 50 lines or factored
  into clear sub-handlers.
- `assistant_answer_lines` no longer takes `_is_streaming`.
- Scroll keys handled in one place.
- `RenderDirty` either documented or migrated to enum.
- `cargo test --workspace` green; clippy clean.
- No bench regression.

## Deferred

- A full state-machine audit of `AgentUiState`. The current
  shape has worked for the existing transitions; a sweep is
  bigger than this PR warrants.
- Refactoring the long autocomplete-context parser
  (Finding F-2). Touching that file pulls in editor logic;
  separate concern.
