# active_input_2026-04-27: drafting and follow-ups while the agent runs

## Investigation summary

Today the input box is visually and functionally locked whenever the
agent is not idle.

Evidence from the current code:

- UI states are coarse: `Idle`, `Streaming`, `ToolExecuting`, and
  `Compacting` (`crates/anie-tui/src/app.rs:198-208`).
- Render treats every non-idle state as locked:
  `let input_locked = !matches!(self.agent_state, AgentUiState::Idle);`
  (`crates/anie-tui/src/app.rs:585`). `InputPane::render` only uses this
  as border styling, but its docs currently describe the editor as not
  accepting input while the agent is running.
- Key dispatch routes active states away from the editor entirely:
  `handle_key_event` calls `handle_active_key` for `Streaming`,
  `ToolExecuting`, and `Compacting` (`crates/anie-tui/src/app.rs:952-965`).
- `handle_active_key` only accepts abort/quit and Home/End scrolling;
  every other key returns `RenderDirty::none()` (`crates/anie-tui/src/app.rs:1016-1051`).
- Even if the TUI sent a prompt during a run, the controller rejects it:
  `UiAction::SubmitPrompt` with `current_run.is_some()` emits
  `"A run is already active..."` instead of starting or queuing
  (`crates/anie-cli/src/controller.rs:307-318`).
- The controller already polls `ui_action_rx` while a run is active
  (`crates/anie-cli/src/controller.rs:134-148`), so it can receive queue
  or abort actions without blocking the active agent task.

The architecture is single-run today: `AgentLoop::run` owns the provider
stream/tool loop until `AgentEnd`. We should not pretend a second user
message can be injected into the same provider HTTP stream. The
practical product shapes are:

1. **Draft while running** — user can type/edit the next message, but it
   is not sent until the current run ends.
2. **Queue follow-up** — pressing Enter while running stores the draft
   and automatically starts it after the current run finishes.
3. **Interrupt and send** — an explicit command/key cancels the current
   run, persists the partial/aborted assistant turn, then starts the
   drafted prompt.

## Principles

1. **Never lose a draft.** Pressing Enter while active must not clear the
   input unless the prompt is definitely queued or submitted.
2. **Ctrl+C remains the abort muscle-memory.** Active typing must not
   steal Ctrl+C / Ctrl+D behavior.
3. **Keep session order honest.** A queued prompt should be appended only
   when it actually starts, after the current run has finished or been
   aborted. Do not write future user messages into the session ahead of
   still-streaming assistant/tool output.
4. **Make the state visible.** If a prompt is queued, the transcript or
   status area should say so. Users should not wonder whether Enter did
   anything.
5. **Start with the safe UX.** Drafting and queueing are compatible with
   the current single-run architecture. True mid-stream conversation
   injection is a later architecture change.

## Planned PRs

| # | Plan | Scope | Outcome |
|---|---|---|---|
| 01 | [Editable active draft](01_editable_active_draft.md) | TUI key dispatch and input styling only | Users can type/edit their next message while the agent works. Enter is guarded so drafts are not lost. |
| 02 | [Queued follow-up prompts](02_queued_followups.md) | TUI action + controller queue | Enter while active queues the draft and auto-starts it after the current run. |
| 03 | [Interrupt-and-send affordance](03_interrupt_and_send.md) | Explicit abort+submit path | Users can turn the active draft into an immediate follow-up by aborting the current run first. |

## Suggested landing order

1. **Plan 01** first. It solves the minimum ask — begin typing the next
   message — without touching controller/session semantics.
2. **Plan 02** next. It adds the actual follow-up queue once the draft UX
   is safe.
3. **Plan 03** last. It is optional but completes the interaction model
   for cases where the agent is going down the wrong path.

## Milestone exit criteria

- [ ] While `AgentUiState::Streaming`, printable keys update the input
      buffer and render urgently.
- [ ] Ctrl+C while active still sends `UiAction::Abort` on first press
      and `UiAction::Quit` on second press.
- [ ] Pressing Enter while active never silently discards the draft.
- [ ] Pressing Enter while active can queue a follow-up after Plan 02.
- [ ] Queued prompts run in FIFO order after the current run, with
      session entries ordered as current assistant/tool output → queued
      user prompt → queued assistant response.
- [ ] A queued prompt supersedes stale automatic retry/backoff behavior
      according to the policy in Plan 02.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.

## Deferred

- True concurrent/mid-stream provider injection. That would require a
  different agent-loop contract (bidirectional stream or controlled
  cancellation + continuation) and is not needed for the immediate UX
  improvement.
- Persisting unsubmitted drafts across process restarts. Useful for a
  long-running agent eventually, but separate from unlocking active
  input.
- Multiple parallel agent runs in one session. This plan preserves the
  current single-run invariant.
