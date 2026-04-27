# 01 — Editable active draft

## Rationale

The smallest useful fix is to let the user type the next message while
the agent is still responding. The current lock is mostly in TUI key
routing, not in `InputPane` itself:

- `InputPane::render` accepts an `input_locked` boolean for border
  styling (`crates/anie-tui/src/input.rs:309-345`). It does not enforce
  read-only behavior.
- `App::handle_key_event` sends every non-idle state to
  `handle_active_key` (`crates/anie-tui/src/app.rs:952-965`).
- `handle_active_key` drops all normal editing keys
  (`crates/anie-tui/src/app.rs:1016-1051`).

So PR 01 can be TUI-local: route active editing keys into the existing
`InputPane`, while preserving active abort/quit and scroll bindings.

## Design

### Active-state key behavior

While the agent is `Streaming`, `ToolExecuting`, or `Compacting`:

- `Ctrl+C` keeps current behavior:
  - first press sends `UiAction::Abort`;
  - second press within 2 seconds sends `UiAction::Quit`.
- `Ctrl+D` keeps current quit behavior.
- `PageUp` / `PageDown` keep scrolling (already shared).
- `Home` / `End` should mirror idle behavior:
  - if the draft buffer is empty, scroll transcript top/bottom;
  - if the draft buffer has content, move within the input line.
- Printable characters, Backspace/Delete, arrows, word movement, and
  multiline insertion should go through `InputPane::handle_key`.
- `Enter` while active must **not** send `SubmitPrompt` yet in PR 01.
  Until Plan 02 lands, it should keep the draft intact and show a small
  system message such as:

  > Agent is still working. Your draft is preserved; wait for the run to
  > finish or press Ctrl+C to abort.

This avoids the current dangerous path where `InputPane::submit()` would
clear the buffer before the controller rejects `SubmitPrompt`.

### Visual state

The input box should no longer look frozen. Options:

1. Rename the render parameter from `input_locked` to something like
   `agent_active` and style active-but-editable with a distinct dim/yellow
   border; or
2. Keep the parameter minimal and pass `false` when the editor is
   editable.

Prefer option 1 if the diff stays small; it makes future queue state
clearer.

Suggested visual states:

- Idle/editable: cyan border (current live style).
- Active/editable draft: yellow or dim-cyan border, with spinner row
  already saying `Responding` / `Running <tool>`.
- Locked should become rare and reserved for overlays/model picker if
  needed.

## Files to touch

- `crates/anie-tui/src/app.rs`
  - Change active key routing to allow editor keys.
  - Guard active `Enter` so it does not clear the draft.
  - Adjust `Home`/`End` active behavior.
  - Adjust input render state passed to `InputPane::render`.
- `crates/anie-tui/src/input.rs`
  - Update render docs/parameter naming if desired.
- `crates/anie-tui/src/tests.rs`
  - Add active-draft tests.

## Phased PRs

### PR A — Route active editing keys to `InputPane`

**Change:**

- Add a helper such as `handle_active_editor_key()`.
- In `handle_active_key`, reserve Ctrl+C/Ctrl+D first.
- For Home/End, use idle's "draft present means editor navigation"
  rule.
- For all ordinary editor keys, call `self.input_pane.handle_key(key)`
  but treat `InputAction::Submit(text)` specially:
  - restore/keep `text` in the input pane, or avoid calling `submit()`
    for Enter while active;
  - emit a non-destructive system message.

Implementation note: because `InputPane::submit()` clears content, it may
be cleaner to intercept `(KeyModifiers::NONE, KeyCode::Enter)` before
calling `handle_key` while active.

**Tests:**

- `active_streaming_accepts_text_input_without_submitting`
- `active_tool_execution_accepts_text_input_without_submitting`
- `enter_while_active_preserves_draft_until_queue_feature_lands`
- `ctrl_c_marks_abort_while_active` and
  `second_ctrl_c_while_active_quits` remain green.
- `home_and_end_preserve_input_editing_when_active_draft_is_present`

**Exit criteria:**

- User can type a next-message draft during active runs.
- Draft is not lost by pressing Enter.
- Abort/quit behavior is unchanged.

### PR B — Active input styling cleanup

**Change:**

- Update `InputPane::render` docs and parameter naming to reflect that
  the editor can be active while the agent runs.
- Optional: use a distinct active-editable border style.

**Tests:**

- Existing snapshots updated only if visual output changes.
- Add a focused render test if using a new border color/style.

**Exit criteria:**

- The UI no longer communicates "frozen" when typing is allowed.

## Test plan

- `cargo test -p anie-tui active`
- `cargo test -p anie-tui ctrl_c`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke:
  - Start a long-running response.
  - Type a sentence into the input box.
  - Press Backspace/Left/Home/End.
  - Press Ctrl+C and confirm abort still works.

## Risks

- Enter handling can accidentally clear the draft. Intercept Enter before
  `InputPane::submit()` while active.
- Autocomplete may open during active slash-command drafts. This is
  probably fine, but tests should cover that typing `/th` while active
  does not dispatch a command accidentally.
- Home/End behavior changes for active state. Match idle semantics to
  minimize surprise.

## Exit criteria

- The input box is editable during active runs.
- No controller/session behavior changes are required in this PR.
- No draft-loss path is introduced.

## Deferred

- Actually queueing/submitting active drafts. That is Plan 02.
