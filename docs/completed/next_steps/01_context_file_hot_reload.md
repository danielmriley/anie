# 01 — Context File Hot-Reload

## Goal

Re-read AGENTS.md / CLAUDE.md before each LLM turn so that mid-session
edits take effect immediately without restart.

## Current behavior

`build_system_prompt()` is called once at startup and on config reload.
The result is stored in `ControllerState.system_prompt` and reused for
every agent run via `build_agent()` → `AgentLoopConfig.system_prompt`.

## Change

Before each agent run (`start_prompt_run` and `start_continuation_run`),
rebuild the system prompt if any context file has changed.

### Implementation

1. Add a `context_files_mtime` field to `ControllerState` that stores the
   max mtime of all context files at the time the system prompt was built.

2. Add a method `ControllerState::refresh_system_prompt_if_needed()` that:
   - Calls `collect_context_files()` and checks the max mtime
   - If different from stored mtime, rebuilds the system prompt via
     `build_system_prompt()` and updates both `system_prompt` and
     `context_files_mtime`
   - Returns whether a rebuild happened (for optional notification)

3. Call `refresh_system_prompt_if_needed()` at the start of
   `start_prompt_run()` and `start_continuation_run()`.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/controller.rs` | Add mtime tracking, refresh method, call before runs |
| `crates/anie-config/src/lib.rs` | Add mtime to `ContextFile` struct |

### Tests

1. Unit test: `refresh_system_prompt_if_needed` detects mtime change
2. Unit test: prompt is not rebuilt when files haven't changed
3. Integration test: agent receives updated system prompt after context
   file change mid-session

### Exit criteria

- [x] Editing AGENTS.md mid-session changes the system prompt on the next turn
- [x] No rebuild happens when files are unchanged (no wasted I/O)
- [x] All existing tests pass
