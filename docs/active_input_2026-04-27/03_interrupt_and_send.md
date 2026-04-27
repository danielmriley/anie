# 03 — Interrupt-and-send affordance

## Rationale

Queued follow-ups are safe, but sometimes the user sees the agent going
in the wrong direction and wants the drafted message to take effect now.
The current escape hatch is manual:

1. Press Ctrl+C to abort.
2. Wait for the run to stop.
3. Type or submit the next prompt.

After Plans 01 and 02, the user may already have a draft in the input
box. Plan 03 adds an explicit way to convert that draft into:

> abort the current run, persist the partial/aborted assistant turn, then
> start this prompt next.

This still preserves the single-run architecture. We are not injecting a
message into an active provider stream; we are canceling and continuing
from a clean session boundary.

## Design

### New action

Add a controller action:

```rust
UiAction::AbortAndQueuePrompt(String)
```

or reuse `QueuePrompt` plus an `Abort` flag if the enum shape is cleaner.
Prefer the explicit variant for testability and log clarity.

### TUI affordance

Choose one primary affordance and one fallback:

1. **Primary:** `Ctrl+Enter` while active sends
   `AbortAndQueuePrompt(draft)`.
   - Caveat: terminal support for `Ctrl+Enter` varies.
2. **Fallback:** active draft slash command:
   - `/interrupt <message>` if entered as a command; or
   - `/send-now` to send the current draft if command parsing can access
     it cleanly.
3. **Alternative if key support is poor:** `Esc` opens a tiny confirm
   prompt for "Abort and send draft?". This is more UI work and should
   be deferred unless needed.

The initial implementation can use a conservative keybinding available
in crossterm tests, but must document terminal limitations.

### Controller behavior

When `AbortAndQueuePrompt(text)` arrives:

- If `current_run.is_some()`:
  - push `text` to the **front** of `queued_prompts`;
  - cancel the current run;
  - emit a system message:

    > Aborting current run; queued draft will send next.

- If `PendingRetry::Armed`:
  - clear the pending retry;
  - start the prompt immediately.
- If idle:
  - start the prompt immediately.

When the canceled run returns, existing `AgentLoop` cancellation behavior
should produce an aborted assistant message. The controller should call
`finish_run()` as usual, then drain the queued prompt. This keeps session
order:

1. original user prompt;
2. partial/aborted assistant message;
3. interrupting user prompt;
4. new assistant response.

## Files to touch

- `crates/anie-tui/src/app.rs`
  - Add active keybinding / command dispatch.
  - Add `UiAction::AbortAndQueuePrompt` variant if chosen.
- `crates/anie-cli/src/controller.rs`
  - Handle abort-and-queue action.
  - Ensure queue drains after canceled run finishes.
- `crates/anie-tui/src/tests.rs`
  - Keybinding/command tests.
- `crates/anie-cli/src/controller_tests.rs`
  - Abort-and-queue ordering tests.
- Optional docs:
  - README/help text for active-input controls.

## Phased PRs

### PR A — Controller abort-and-front-queue action

**Change:**

- Add explicit `UiAction::AbortAndQueuePrompt(String)`.
- Implement controller handling:
  - active run → front-queue + cancel;
  - pending retry → clear retry + start;
  - idle → start.

**Tests:**

- `abort_and_queue_cancels_current_run_and_starts_prompt_after_abort`
- `abort_and_queue_during_pending_retry_clears_retry`
- Session contains aborted assistant before interrupting user prompt.

**Exit criteria:**

- Controller behavior works independently of final TUI keybinding.

### PR B — TUI affordance

**Change:**

- Add the chosen active keybinding or command.
- The draft should clear only after the action is sent.
- Empty draft should not abort-and-queue; it should either no-op or fall
  back to ordinary abort prompt behavior.

**Tests:**

- Active draft + keybinding sends `AbortAndQueuePrompt`.
- Empty active draft + keybinding does not cancel unexpectedly.
- Ctrl+C behavior remains unchanged.

**Exit criteria:**

- Users can intentionally send a drafted correction immediately.

### PR C — Help/status documentation

**Change:**

- Update `/help` or status messaging to mention:
  - type while agent runs to draft;
  - Enter queues follow-up;
  - chosen interrupt-and-send shortcut.

**Tests:**

- Help snapshot/format test updated if one exists.

**Exit criteria:**

- The feature is discoverable.

## Test plan

- `cargo test -p anie-tui interrupt`
- `cargo test -p anie-cli abort_and_queue`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke:
  - Start a long-running/tool-using response.
  - Type "Actually, do X instead".
  - Trigger interrupt-and-send.
  - Confirm current run aborts and the new prompt starts.

## Risks

- `Ctrl+Enter` may not be reported by all terminals. Keep a command
  fallback or document limitations.
- Aborting during tool execution depends on tool cancellation support.
  If a tool ignores cancellation, the queued prompt waits until the tool
  returns. This is consistent with current abort semantics and should be
  improved by the web/tool cancellation plans separately.
- Duplicating queue-drain logic can start prompts twice. Reuse the
  helper from Plan 02.

## Exit criteria

- There is a deliberate, tested "send now" path.
- It preserves session ordering and current cancellation semantics.
- It does not replace Ctrl+C; it complements it.

## Deferred

- True live provider-stream steering without cancellation.
- Confirm-dialog UI unless keybinding/command affordances prove too easy
  to trigger accidentally.
