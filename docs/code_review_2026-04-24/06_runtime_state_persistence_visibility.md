# 06 — Runtime-state persistence visibility

## Rationale

Runtime provider/model/thinking changes update in-memory state and then
attempt to save runtime state. The review found failures are logged but
not surfaced. That means the UI can appear to accept a setting, while
the next launch silently reverts because `~/.anie/state.json` could not
be written.

Codex sometimes logs and continues for optional persistence, but anie's
typed error style is stronger. This plan keeps the non-fatal nature of
runtime-state persistence while making failure visible to the user and
callers.

## Design

`ConfigState::persist_runtime_state` should return a result that callers
must handle.

Behavior by mode:

- Interactive/TUI: apply the in-memory change, then append or emit a
  non-fatal system warning if persistence fails.
- Print/RPC: include a warning in logs and any structured result surface
  that already exists for non-fatal runtime warnings.
- Tests: failure injection should assert the warning path, not just log
  output.

This plan should not make runtime-state persistence failure roll back
the in-memory setting. The user's current session should continue with
the requested state; only persistence across launches failed.

## Files to touch

- `crates/anie-cli/src/runtime/config_state.rs`
  - Change `persist_runtime_state` to return `Result<()>`.
  - Preserve in-memory update before save attempt unless a better
    rollback policy is explicitly chosen.
- `crates/anie-cli/src/controller.rs`
  - Surface non-fatal persistence warnings for model/thinking/provider
    changes.
- `crates/anie-cli/src/user_error.rs` or message helpers
  - Reuse existing non-fatal warning patterns if present.
- `crates/anie-cli/src/controller_tests.rs`
  - Add failure-injection tests.

## Phased PRs

### PR A — Return persistence results

**Change:**

- Update `ConfigState::persist_runtime_state` signature.
- Propagate `save_runtime_state` errors with context.
- Update all call sites to handle the result.

**Tests:**

- Existing successful model/thinking persistence tests still pass.
- A mocked or temp-dir permission failure returns an error.

**Exit criteria:**

- Runtime persistence failures are no longer log-only inside
  `ConfigState`.

### PR B — Surface non-fatal warnings in controller paths

**Change:**

- When a runtime setting is applied but persistence fails, emit a system
  message explaining that the setting is active for this session but may
  not persist.
- Keep logs with path/error context.

**Tests:**

- Model change with persistence failure records a warning.
- Thinking change with persistence failure records a warning.
- The in-memory state still changes.

**Exit criteria:**

- Users can see persistence failures without digging through logs.

## Test plan

- `cargo test -p anie-cli runtime_state`
- `cargo test -p anie-cli controller`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke: point runtime state at an unwritable location and change
  model/thinking; confirm warning is visible and session state changes
  in memory.

## Risks

- Avoid turning a non-fatal persistence problem into a failed model
  change unless explicitly desired.
- A setting can be active in memory, fail to persist, and then silently
  revert on the next launch if the user quits before noticing the
  warning; the warning copy should make this persisted-then-quit window
  explicit.
- Make sure warning messages do not leak sensitive paths beyond normal
  config path expectations.
- Avoid duplicating warnings on every render/frame; emit once per
  failed setting change.

## Exit criteria

- Runtime-state persistence failures are surfaced where users can act.
- In-memory runtime changes continue to work when persistence fails.
