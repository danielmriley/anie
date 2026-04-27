# 03 — Retry-state consistency for model/thinking changes

## Rationale

The controller arms retry backoff after retryable failures. While a
retry is armed, UI actions can still arrive. The review found that
model and thinking changes are rejected only when a run is actively in
progress; during `PendingRetry::Armed`, those changes can be accepted.
The retry continuation then builds a new agent from current controller
state, so the retry can run with different model/thinking settings than
the failed attempt that scheduled it.

pi's event model generally applies runtime changes to the next prompt
rather than mutating an in-flight retry. Codex copies runtime-only turn
state into child/continuation configs when needed. For anie, the right
fix is to make the retry boundary explicit.

## Design

Choose one behavior and enforce it consistently:

**Recommended behavior:** model/provider/thinking changes cancel an
armed retry and are treated as a user-directed state change for the
next prompt.

Rationale:

- It matches user intent: changing model or thinking while waiting for a
  retry usually means "do not keep retrying the old failed run."
- It avoids rejecting harmless settings work while the UI is idle.
- It keeps session auditability clear if the cancellation is recorded as
  a system marker.

Alternative behavior: reject model/provider/thinking changes while a
retry is armed, using the same UX as active-run rejection. This is
simpler but less flexible.

The implementation should cover every run-affecting action:

- model changes
- resolved model changes
- provider changes if represented separately
- thinking level changes
- any future runtime option that affects provider request shape

## Files to touch

- `crates/anie-cli/src/controller.rs`
  - Centralize "run-affecting config change" handling.
  - Cancel or reject armed retry before applying the change.
  - Add a session/system event that explains what happened.
- `crates/anie-cli/src/controller_tests.rs`
  - Add tests for armed retry + `SetModel`.
  - Add tests for armed retry + `SetThinking`.
  - Add tests for the chosen cancellation/rejection message.

## Phased PRs

### PR A — Define and enforce retry-state policy

**Change:**

- Add a helper like `handle_run_affecting_change_during_retry`.
- If using the recommended cancellation policy:
  - clear `PendingRetry::Armed`
  - record a concise system message
  - apply the requested config change
- If using the rejection policy:
  - leave retry armed
  - reject the action with a system message
  - keep runtime state unchanged

**Tests:**

- `SetModel` during armed retry behaves according to policy.
- `SetResolvedModel` during armed retry behaves according to policy.
- `SetThinking` during armed retry behaves according to policy.

**Exit criteria:**

- A retry continuation cannot silently inherit a different
  model/thinking state than the failed attempt.

### PR B — Audit other run-affecting runtime options

**Change:**

- Search controller actions for any option that affects provider stream
  construction.
- Route those actions through the same armed-retry policy.

**Tests:**

- Add targeted tests for any additional action found.

**Exit criteria:**

- The policy is centralized enough that future runtime options are hard
  to forget.

## Test plan

- `cargo test -p anie-cli controller`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke: trigger a retryable provider error, change model during
  backoff, and confirm the retry is canceled or the change is rejected
  according to the documented policy.

## Risks

- Canceling retry without a visible marker would look like a lost
  retry. Always surface the cancellation.
- Rejecting changes can feel broken if the UI appears idle during
  backoff. If rejection is chosen, make the message clear and include
  how to cancel retry.
- Do not clear retry state for display-only settings.

## Exit criteria

- Retry behavior is deterministic and auditable.
- Controller tests cover model and thinking changes during armed retry.

