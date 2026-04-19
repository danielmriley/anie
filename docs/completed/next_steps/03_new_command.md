# 03 — `/new` Command

## Goal

Add a `/new` command that starts a fresh session without restarting anie.

## Current behavior

To start a fresh session, the user must quit anie and relaunch it.
The `/session` command can switch to an existing session but cannot
create a new one.

## Change

### Implementation

1. Add a `NewSession` variant to `UiAction`.

2. Handle `/new` in `App::handle_slash_command()`:
   - Send `UiAction::NewSession` to the controller

3. Handle `UiAction::NewSession` in the controller:
   - Block if a run is active
   - Create a new `SessionManager` via `SessionManager::new_session()`
   - Replace `self.state.session` with the new session
   - Clear the transcript in the TUI via `AgentEvent::TranscriptReplace`
     with an empty message list
   - Send a status event and system message confirming the new session
   - Persist runtime state with the new session ID

4. Add `/new` to the help text.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/app.rs` | Handle `/new`, add `UiAction::NewSession`, update help |
| `crates/anie-cli/src/controller.rs` | Handle `UiAction::NewSession` |

### Tests

1. Unit test in `anie-tui`: `/new` sends `UiAction::NewSession`
2. Controller test: new session creates a fresh session file and clears
   transcript

### Exit criteria

- [x] `/new` creates a fresh session
- [x] Transcript is cleared in the TUI
- [x] Old session remains on disk (accessible via `/session list`)
- [x] Status bar updates with new session info
- [x] Cannot `/new` while a run is active
- [x] Help text includes `/new`
- [x] All existing tests pass
