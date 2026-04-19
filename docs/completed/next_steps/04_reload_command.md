# 04 — `/reload` Command

## Goal

Add a `/reload` command that hot-reloads config and context files
without restarting anie.

## Current behavior

Config reload exists internally — triggered by `UiAction::ReloadConfig`
after onboarding/provider-management changes. But there is no user-facing
command to trigger it on demand.

Context file reload happens as part of config reload (the system prompt
is rebuilt). After item 01 lands, context files also refresh per-turn.
This command provides an explicit manual trigger.

## Change

### Implementation

1. Handle `/reload` in `App::handle_slash_command()`:
   - Send `UiAction::ReloadConfig { provider: None, model: None }`

2. The controller already handles `UiAction::ReloadConfig`:
   - Reloads config from disk
   - Rebuilds model catalog
   - Rebuilds system prompt (including context files)
   - Sends status event
   - Sends "Configuration reloaded." message

   No controller changes needed — the existing handler does exactly
   what `/reload` needs.

3. Add `/reload` to the help text.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/app.rs` | Handle `/reload`, update help text |

### Tests

1. Unit test in `anie-tui`: `/reload` sends `UiAction::ReloadConfig`
   with both fields `None`

### Exit criteria

- [x] `/reload` reloads config and context files
- [x] Status bar updates with any model/provider changes
- [x] System message confirms reload
- [x] Cannot `/reload` while a run is active (existing guard)
- [x] Help text includes `/reload`
- [x] All existing tests pass
