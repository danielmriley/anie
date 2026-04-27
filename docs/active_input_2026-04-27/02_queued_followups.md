# 02 — Queued follow-up prompts

## Rationale

After Plan 01, users can draft while the agent runs, but Enter still
cannot submit without racing the active run. The controller currently
rejects active submissions:

- `crates/anie-cli/src/controller.rs:307-318` — `SubmitPrompt` while
  `current_run.is_some()` emits "A run is already active".

The controller is nevertheless already structured to receive UI actions
while a run is active:

- `crates/anie-cli/src/controller.rs:134-148` — the active-run `select!`
  polls `ui_action_rx` and the agent task concurrently.

So we can add a controller-owned FIFO queue for follow-up prompts. The
TUI submits drafts into that queue while active, and the controller
starts them when the current run reaches a safe boundary.

## Design

### New action

Add an explicit TUI/controller action:

```rust
UiAction::QueuePrompt(String)
```

Why not reuse `SubmitPrompt`?

- `SubmitPrompt` currently means "start a prompt now if possible" and is
  used by print/RPC paths.
- `QueuePrompt` makes active TUI behavior explicit and avoids surprising
  non-TUI clients that may rely on today's rejection.

Optionally, after the queue behavior has proven itself, RPC can also opt
into queueing explicitly.

### Controller queue

Add to `InteractiveController`:

```rust
queued_prompts: VecDeque<String>
```

Behavior:

- `QueuePrompt(text)` while a run is active pushes `text` onto the back
  of the queue and emits a system message:

  > Queued follow-up #N. It will send after the current run finishes.

- `QueuePrompt(text)` while idle can either:
  - start immediately via `start_prompt_run(text)`, or
  - push then drain the queue. Prefer the helper path so behavior is
    consistent.
- After a run finishes and `finish_run()` has persisted generated
  messages, drain one queued prompt by calling `start_prompt_run(next)`.
  This preserves session order.
- If multiple prompts are queued, run them one at a time in FIFO order.

### Retry/backoff policy

Queued user input is a context-changing signal. It should not be hidden
behind stale automatic retries.

Policy for this plan:

- If a queued prompt exists when an error would schedule a transient
  retry, **do not arm the retry**. Finish/persist the failed run, emit a
  system message such as:

  > Skipping automatic retry because a follow-up is queued.

  Then start the queued prompt.
- If a retry is already pending and `QueuePrompt` arrives, clear
  `PendingRetry` and start/queue the prompt immediately.
- Context-overflow compaction before a queued prompt can still happen in
  `start_prompt_run()` through the existing `maybe_auto_compact()` path.

This matches existing controller behavior where a fresh prompt cancels a
pending retry in `start_prompt_run()`.

### TUI behavior

When active and the user presses Enter on a non-empty non-slash draft:

1. `InputPane` returns the draft.
2. `App` sends `UiAction::QueuePrompt(text)` instead of
   `SubmitPrompt(text)`.
3. The input clears only after sending the queue action.
4. The controller emits the queued system message.

Slash commands while active need explicit handling:

- UI-local commands like `/clear`, `/markdown`, and `/tool-output` can
  continue to dispatch immediately if current handlers are safe.
- Run-affecting commands (`/model`, `/thinking`, `/context-length`,
  `/compact`, `/session`, etc.) already have controller-side or TUI-side
  active-run guards. Keep those guards.
- Do not queue slash commands as model prompts.

## Files to touch

- `crates/anie-tui/src/app.rs`
  - Add `UiAction::QueuePrompt` variant.
  - Active Enter sends `QueuePrompt` for ordinary drafts.
  - Optional local queued-count hint if desired.
- `crates/anie-cli/src/controller.rs`
  - Add `queued_prompts: VecDeque<String>`.
  - Handle `UiAction::QueuePrompt`.
  - Drain queue after current run completion.
  - Define queued-prompt vs retry/backoff behavior.
- `crates/anie-tui/src/tests.rs`
  - Active Enter sends `QueuePrompt` and clears draft.
- `crates/anie-cli/src/controller_tests.rs`
  - Queue execution/order tests.

## Phased PRs

### PR A — TUI sends queued prompt action while active

**Change:**

- Add `UiAction::QueuePrompt(String)`.
- In active state, Enter on a normal draft sends `QueuePrompt`.
- Keep Enter on empty draft as no-op.
- Keep slash commands on the existing command path, with active guards.

**Tests:**

- `enter_while_streaming_sends_queue_prompt_and_clears_draft`
- `enter_while_active_empty_draft_is_noop`
- `active_slash_clear_still_clears_locally` or equivalent for one safe
  UI-local command.

**Exit criteria:**

- The TUI no longer sends `SubmitPrompt` for active follow-up drafts.

### PR B — Controller FIFO queue

**Change:**

- Add `queued_prompts` to `InteractiveController`.
- Implement `queue_prompt(text)` and `start_next_queued_prompt()`
  helpers.
- After run completion and `finish_run()`, start the next queued prompt
  if no retry/compaction continuation is active.
- Preserve print mode: `exit_after_run` should exit only when no current
  run, no pending retry, and no queued prompts remain.

**Tests:**

- `queued_prompt_runs_after_current_run_finishes`
- `queued_prompts_run_fifo`
- `queued_prompt_is_persisted_after_current_assistant_message`
- `exit_after_run_waits_for_queued_prompt_or_queue_action_not_used_in_print_mode`
  (choose the test that matches final semantics).

**Exit criteria:**

- Queued prompts execute automatically and in order.
- Session ordering is correct.

### PR C — Queue vs retry policy

**Change:**

- If queued prompts exist when retry policy returns `Retry`, skip arming
  `PendingRetry`, finish the failed run, and start the queue.
- If `QueuePrompt` arrives while `PendingRetry::Armed`, clear the retry
  and start/queue the prompt.

**Tests:**

- `queued_prompt_suppresses_transient_retry_after_current_error`
- `queue_prompt_during_pending_retry_cancels_retry_and_starts_prompt`
- Existing retry cancellation tests remain green.

**Exit criteria:**

- New user input does not wait behind stale automatic retry/backoff.

### PR D — Queue visibility polish

**Change:**

- Emit system messages when prompts are queued and when a queued prompt
  starts.
- Optional: include queue count in status bar later, but do not block the
  feature on status-bar redesign.

**Tests:**

- Controller emits a visible queued message.
- TUI renders queued messages as system messages.

**Exit criteria:**

- Users can tell Enter queued the follow-up.

## Test plan

- `cargo test -p anie-tui queue`
- `cargo test -p anie-cli queue`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke:
  - Start a long answer.
  - Type a follow-up and press Enter.
  - Confirm the draft clears and a queued message appears.
  - Confirm the follow-up starts after the current answer ends.

## Risks

- Queueing slash commands as prompts would be surprising. Keep slash
  handling separate.
- Retry/queue interactions can create subtle duplicate starts. Centralize
  queue draining in one helper and call it from well-defined run-loop
  points.
- If the current run never ends and the user does not abort, queued
  prompts wait forever. That is expected under the single-run invariant;
  Plan 03 provides an interrupt path.

## Exit criteria

- Enter while active queues a prompt instead of rejecting or losing it.
- Queued prompts run FIFO after safe boundaries.
- User-visible status makes queueing obvious.

## Deferred

- Persisting the queue to disk. If anie crashes before a queued prompt
  starts, the draft is lost in this plan.
- RPC queue support unless explicitly opted in.
