# run_abort_breadcrumb_2026-04-28: error breadcrumbs for canceled retries

## Rationale

When a provider request fails transiently (e.g. `Rate limited`) and the
retry is canceled before its backoff deadline fires — typically because
the user submits a new prompt, switches providers, or aborts — the
failed run's user prompt remains in the session log but **no assistant
breadcrumb is written for that turn**. The session ends up with
consecutive user messages and no record of what happened to the first
one.

### Concrete reproduction

Captured 2026-04-28 from `~/.anie/sessions/8a865607.jsonl` while
testing OpenRouter's heavily rate-limited `minimax/minimax-m2.5:free`
model:

```text
user      "Hello!"                                ← rate-limited, retry pending
user      "Are you there?"                        ← typed before retry fired
assistant "Hello! Yes, I'm here…"  (openrouter)   ← only this one replies
user      "What happened on the first turn?"      ← rate-limited again
user      "Hello"                                 ← typed after switching to ollama
assistant "In the first turn, you said Hello!…"   ← recount is wrong
```

Two pairs of consecutive user messages with no assistant message
between them. The corresponding `anie.log.2026-04-28` entries:

```text
20:57:39  starting interactive run  provider=openrouter  model=minimax/minimax-m2.5:free
20:57:40  scheduling transient provider retry  delay_ms=12268  error=Rate limited
20:57:48  starting interactive run  provider=openrouter  model=minimax/minimax-m2.5:free
20:57:51  persisting completed run  generated_messages=1
```

The retry was scheduled with a 12-second backoff. The user typed a new
prompt 8 seconds in. The pending retry was correctly canceled, but the
first run's failure was never finalized into the session.

When a different model (ollama qwen3.5:9b after a manual switch) takes
over later in the same session, it inherits the irregular transcript
and reconstructs history incorrectly — which is what made its recount
of the "first turn" look subtly wrong.

### What the controller does today

`InteractiveController::pending_retry: PendingRetry` holds the backoff
state between runs:

```rust
enum PendingRetry {
    Idle,
    Armed { deadline: Instant, attempt: u32, already_compacted: bool },
}
```

Cancellation paths (e.g. `UiAction::SubmitPrompt`, `Abort`,
`Quit`, slash-command-driven session changes) transition `Armed →
Idle` directly without persisting anything for the failed run. The
state machine is correct; the gap is purely in the session log.

## Design

When the controller transitions `PendingRetry::Armed → Idle` for any
reason **other than the deadline firing**, it should first finalize
the canceled run as a failed turn:

1. Build a synthetic error-assistant message via the existing
   `ControllerState::error_assistant_message` helper, carrying:
   - `stop_reason: StopReason::Error`
   - `error_message: Some(<ProviderError>.to_string())` — the same
     string the user already saw rendered as the retry reason.
   - `provider` and `model` from the just-canceled run.
2. Persist it through the same `finish_run` path the deadline-fired
   retry would have used had it ultimately exhausted attempts.
3. Then proceed with the user's action.

This keeps the session-log invariant: **every user message has a
following assistant message** — success, error, or aborted.

### Why this matches the existing failure pattern

The retry-exhausted path (`RetryDecision::GiveUp` /
`AttemptsExhausted`) already writes an error-assistant message
through `finish_run`. We're extending the same shape to the
"canceled before retrying" case. The existing rendering and
session-log machinery is reused as-is.

### Trapped state we need to carry

The `Armed` variant carries `attempt` and `already_compacted` but
not the original `ProviderError` or the `provider`/`model` it failed
on. To build the breadcrumb we need either:

- (A) extend `PendingRetry::Armed` with `error: ProviderError`,
  `provider: String`, `model: String`, OR
- (B) capture this in a separate field on `InteractiveController`
  alongside `pending_retry`.

Recommend (A): one source of truth, lifecycle is identical to the
existing retry state, no risk of the two getting out of sync. The
struct grows by ~3 owned fields; the variant remains
representable as plain data.

## Files to touch

- `crates/anie-cli/src/controller.rs`
  - Extend `PendingRetry::Armed` with `error: ProviderError`,
    `provider: String`, `model: String`. Update construction at
    the `RetryDecision::Retry` branch to populate them.
  - Add `abort_pending_retry(&mut self) -> Option<finalized
    AssistantMessage>` helper that, when `Armed`, builds the
    breadcrumb, calls the existing finalize path, and returns
    `Idle`.
  - Call the helper from every cancel path:
    - `UiAction::SubmitPrompt` (top of `start_prompt_run` /
      wherever `PendingRetry::Idle` is set on new submission)
    - `UiAction::QueuePrompt` — actually no, queued prompts wait
      for the run boundary; the queue mechanic already preserves
      the retry. Verify this is the case before touching it.
    - `UiAction::Abort`
    - `UiAction::Quit`
    - `new_session`, `switch_session`, `fork_session`
    - model/provider switches (`/model`, `/provider`)
- `crates/anie-cli/src/controller_tests.rs`
  - Tests for each cancel path.

## Phased PRs

### PR A — Carry failure context on `PendingRetry::Armed`

Pure refactor: extend the variant, populate at the existing retry-
schedule call site, no behavior change. Lands first so PR B can
land without churn.

**Tests:** existing retry tests still pass; one new test asserts
the new fields are populated correctly when a retry is scheduled.

### PR B — Write breadcrumb on user-driven cancel

Add `abort_pending_retry` and wire into `SubmitPrompt`, `Abort`,
`Quit`. Tests for each.

### PR C — Session-change paths

Wire into `new_session`, `switch_session`, `fork_session`, and the
slash-command-driven model switch. Decide whether a model switch
mid-pending-retry should trip the breadcrumb (probably yes — the
old run will never recover and the new model will inherit the
transcript).

## Test plan

Per-PR tests, plus end-to-end:

- `pending_retry_canceled_by_new_prompt_writes_error_assistant`
- `pending_retry_canceled_by_abort_writes_error_assistant`
- `pending_retry_canceled_by_quit_writes_error_assistant`
- `pending_retry_canceled_by_new_session_writes_error_assistant`
- `pending_retry_canceled_by_model_switch_writes_error_assistant`
- `pending_retry_fired_by_deadline_writes_no_extra_breadcrumb`
  (regression guard — the existing retry path stays unchanged)
- `session_log_invariant_no_consecutive_user_messages_under_cancel`
  (read-after-write check on the persisted session)

The session-log invariant test is the load-bearing one: it
captures the user-visible bug rather than implementation details.

## Risks

- **Double-finalization.** If the deadline fires concurrently with
  a cancel (a tight tokio race), we could finalize twice and end
  up with two assistant messages for one user prompt. The
  existing `PendingRetry::Armed → Idle` transition is the
  serialization point; the helper only runs when state is
  `Armed`, so the second caller observes `Idle` and skips. Verify
  with a stress test if the race feels real.
- **Budget interaction.** PR 8.2's `compactions_remaining_this_turn`
  decrements on successful compactions, not retries. The new
  breadcrumb path doesn't touch compaction budgets, but verify
  in tests that aborting a pending retry doesn't accidentally
  consume a budget slot.
- **TUI cosmetic.** The new error-assistant message will render in
  the transcript. The existing error-assistant rendering path
  (used for `AttemptsExhausted` and stream errors) handles this
  shape today, so no new rendering work — but smoke-check that
  back-to-back error-assistants don't render oddly when a user
  rage-types through several rate-limited turns.
- **OpenRouter-specific blast radius.** This bug only shows up when
  retries are common, which today means free-tier OpenRouter
  models. The fix is correct for any provider that produces
  transient errors, but acknowledge the trigger-population is
  small.

## Exit criteria

- [ ] Session log invariant holds: no two consecutive user
      messages without an intervening assistant message under any
      retry-cancel path.
- [ ] All five cancel paths (new prompt, abort, quit, session
      change, model switch) covered by tests.
- [ ] Existing deadline-fires-retry path unchanged (regression
      test green).
- [ ] `cargo test --workspace`, `cargo clippy --workspace
      --all-targets -- -D warnings`, `cargo fmt --all -- --check`
      clean.
- [ ] Manual repro: rate-limit a turn against
      `openrouter/minimax-m2.5:free` (or any free-tier model
      where rate limits are easy to trigger), type a new prompt
      during backoff, confirm the session log shows the
      error-assistant breadcrumb and a follow-on model
      reconstructs history correctly.

## Deferred

- **Live UI feedback when retry is canceled.** The TUI already
  shows "Retrying" on `RetryScheduled`. We could also show
  "Retry canceled" in the activity row when the user cancels.
  Speculative; the transcript breadcrumb is the load-bearing fix.
- **Distinguishing rate-limit from other transient errors in the
  breadcrumb's structured fields.** The `error_message` string
  already carries this. A dedicated `StopReason::RateLimited`
  variant could land later if classification matters for replay
  fidelity or analytics.
- **Reactive backoff for free-tier OpenRouter models.** Plan 02
  covered the per-turn compaction budget; an analogous
  per-session retry budget for repeated rate-limit failures
  could short-circuit the second-attempt-also-fails pattern
  observed in the captured session. Out of scope here — separate
  plan if real workloads keep hitting it.
