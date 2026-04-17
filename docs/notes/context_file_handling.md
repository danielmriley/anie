# Context File Handling

## Summary

Make AGENTS.md / CLAUDE.md context files reactive — edits mid-session
should take effect without restarting anie.

## Current State

Context files are read once at startup via `build_system_prompt()` in
`controller.rs`. The resulting string is stored in `ControllerState.system_prompt`
and reused for every LLM call. Files are also re-read on config reload,
but not between turns.

Edits to AGENTS.md mid-session have no effect until `/reload` or restart.

## Action Items

### 1. Re-read before each LLM call
Before building the `LlmContext` for a new agent turn, check whether any
context files have changed since the last read. If so, rebuild the system
prompt.

Implementation options:
- **Stat-based**: check file mtime before each turn. Cheap, reliable.
- **Watch-based**: use `notify` crate to watch for changes. More complex,
  but avoids per-turn I/O.
- **Hybrid**: stat-check on each turn, with an optional watcher for
  instant feedback in the TUI (e.g. "Context files reloaded").

Stat-based is the simplest and probably sufficient.

### 2. Rebuild system prompt on change
When a change is detected:
- Re-run `collect_context_files()` with the current config
- Rebuild the system prompt
- Update `ControllerState.system_prompt`
- Optionally notify the user via TUI status message

### 3. Consider a pre-turn hook
A more general solution would be a hook that runs before each LLM call,
allowing context files (and potentially other state) to be refreshed.
This mirrors pi's `context` event that fires before each call.

Not needed now, but the context-file refresh should be structured so
that a future hook system can subsume it cleanly.

## Priority

Medium — this is a frequent papercut. Users edit AGENTS.md and wonder
why changes aren't reflected until they restart.
