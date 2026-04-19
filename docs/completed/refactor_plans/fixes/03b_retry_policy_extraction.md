# Fix 03b — `RetryPolicy::decide` extraction + plan 05 reconciliation

Closes out plan 03 phase 4 (the part that wasn't done) and
reconciles plan 05's Design Principle 2 ("retryability is not in
the error type") with the `ProviderError::is_retryable()` method
that shipped on the enum.

## Motivation

Plan 03 phase 4 promised a pure `RetryPolicy::decide(error,
attempt, already_compacted) -> RetryDecision` function, covered
by seven unit tests. What landed was only `RetryConfig` and
`retry_delay_ms` — both useful, but the decision tree is still
inlined in the print-mode event loop (`controller.rs:351–371`,
`836–850`).

At the same time, plan 05 added
`ProviderError::is_retryable()` and `ProviderError::retry_after_ms()`
as methods directly on `ProviderError`. Plan 05's Design Principle
2 explicitly states this was not the intended shape:

> **Retryability is not in the error type.** It's a property
> derived by `RetryPolicy` (plan 03, phase 4). Errors stay
> descriptive; retry decisions stay in one place.

Today we have three places involved in the retry decision:

1. `ProviderError::is_retryable()` — a Boolean derived from the
   variant.
2. `retry_delay_ms(...)` — delay derived from `RetryConfig` and
   `Retry-After` hint.
3. `controller.rs::should_retry_transient`,
   `should_retry_after_overflow`, and their interleaved callers —
   the glue that decides which action to take.

The code works, but anyone reading the retry logic has to stitch
together three files. Worse, plan 10 phase 4 (extension-registered
providers) and the extension host's own retry policy cannot reuse
the current shape because it's baked into the `InteractiveController`.

## Design principles

1. **One decision function.** Every retry decision goes through
   `RetryPolicy::decide`. Callers match on the returned
   `RetryDecision` and execute the action — they never re-derive
   "should I retry this error" themselves.
2. **Pure.** `decide` takes borrowed state and returns a decision.
   No I/O, no event emission.
3. **Behavior-preserving.** This is an extraction, not a semantic
   change. Same error → same decision today.
4. **Eventually move retryability off `ProviderError`.** Phase 3
   of this plan does that after the caller side has migrated.

## Preconditions

- Plan 05 landed (`ProviderError` new variants, with
  `is_retryable` + `retry_after_ms` methods).
- Plan 03 phases 1, 2 landed.
- `crates/anie-cli/src/retry_policy.rs` exists with `RetryConfig`
  and `retry_delay_ms`.

---

## Phase 1 — Add `RetryPolicy::decide` + `RetryDecision`

**Goal:** New pure decision function, fully unit-tested. Nothing
else calls it yet.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/retry_policy.rs` | Add `RetryDecision` enum + `RetryPolicy::decide(...)` + companion `RetryPolicy::delay_for(...)` wrapping today's `retry_delay_ms` |

### Sub-step A — Type shape

```rust
/// What to do about a `ProviderError` that ended a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetryDecision {
    /// Retry the same request after the given delay.
    Retry { attempt: u32, delay_ms: u64 },

    /// Compact the session and then retry. Used for
    /// `ContextOverflow` on the first attempt.
    Compact,

    /// Stop retrying — either the error is terminal or we've
    /// exhausted attempts.
    GiveUp { reason: GiveUpReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GiveUpReason {
    /// Error is inherently terminal (auth, request-build,
    /// tool-call malformed, native-reasoning unsupported).
    Terminal,
    /// Already tried to compact once this run and still overflowed.
    AlreadyCompacted,
    /// Hit the configured transient-retry limit.
    AttemptsExhausted,
}

/// Pure retry-decision function.
pub(crate) struct RetryPolicy<'a> {
    pub(crate) config: &'a RetryConfig,
}

impl<'a> RetryPolicy<'a> {
    /// Decide what to do with `error` after `attempt` transient
    /// retries. `already_compacted` indicates whether the current
    /// run has already been through a compaction attempt.
    pub(crate) fn decide(
        &self,
        error: &ProviderError,
        attempt: u32,
        already_compacted: bool,
    ) -> RetryDecision {
        match error {
            // Context overflow drives compaction on first hit, give
            // up on repeat.
            ProviderError::ContextOverflow(_) => {
                if already_compacted {
                    RetryDecision::GiveUp { reason: GiveUpReason::AlreadyCompacted }
                } else {
                    RetryDecision::Compact
                }
            }
            // Terminal errors.
            ProviderError::Auth(_)
            | ProviderError::RequestBuild(_)
            | ProviderError::ToolCallMalformed(_)
            | ProviderError::NativeReasoningUnsupported(_) => {
                RetryDecision::GiveUp { reason: GiveUpReason::Terminal }
            }
            // Transient: retry if we have attempts left.
            ProviderError::RateLimited { .. }
            | ProviderError::Transport(_)
            | ProviderError::EmptyAssistantResponse
            | ProviderError::InvalidStreamJson(_)
            | ProviderError::MalformedStreamEvent(_) => {
                if attempt >= self.config.max_retries {
                    RetryDecision::GiveUp { reason: GiveUpReason::AttemptsExhausted }
                } else {
                    RetryDecision::Retry {
                        attempt: attempt + 1,
                        delay_ms: self.delay_for(error, attempt + 1),
                    }
                }
            }
            ProviderError::Http { status, .. } => {
                if matches!(status, 429 | 500 | 502 | 503 | 529) && attempt < self.config.max_retries {
                    RetryDecision::Retry {
                        attempt: attempt + 1,
                        delay_ms: self.delay_for(error, attempt + 1),
                    }
                } else {
                    RetryDecision::GiveUp {
                        reason: if attempt >= self.config.max_retries {
                            GiveUpReason::AttemptsExhausted
                        } else {
                            GiveUpReason::Terminal
                        },
                    }
                }
            }
        }
    }

    /// Compute the delay for a retry (exponential backoff + jitter).
    /// Exposed separately so callers that already know they're
    /// retrying can reuse it.
    pub(crate) fn delay_for(&self, error: &ProviderError, attempt: u32) -> u64 {
        retry_delay_ms(self.config, error, attempt)
    }
}
```

### Sub-step B — Seven + unit tests

Plan 03 phase 4 specified seven tests:

| # | Test name |
|---|---|
| 1 | `auth_error_gives_up_immediately` |
| 2 | `rate_limit_returns_retry_with_backoff` |
| 3 | `context_overflow_triggers_compact_on_first_attempt` |
| 4 | `context_overflow_gives_up_if_already_compacted` |
| 5 | `http_5xx_retries_up_to_limit` |
| 6 | `http_4xx_gives_up` |
| 7 | `stream_error_retries_limited_times` |

Plus two additional cases to cover the new variants from plan 05:

| 8 | `tool_call_malformed_gives_up_as_terminal` |
| 9 | `native_reasoning_unsupported_gives_up_as_terminal` |

Each test constructs `RetryConfig::default()` (with jitter=false
for deterministic assertion), a `RetryPolicy { config: &config }`,
and an error/attempt pair. Asserts on the exact `RetryDecision`.

### Sub-step C — No caller migration yet

Phase 1 leaves `should_retry_transient`,
`should_retry_after_overflow`, and the inline decision code in
`controller.rs` untouched. Phase 2 migrates them.

### Exit criteria

- [ ] `RetryPolicy::decide` exists, pure, and unit-tested.
- [ ] 9 unit tests pass.
- [ ] No caller migration yet.

---

## Phase 2 — Migrate callers in `controller.rs`

**Goal:** `should_retry_transient`,
`should_retry_after_overflow`, and the inline event-loop retry code
all go through `RetryPolicy::decide`. The helpers on
`ControllerState` either disappear or become thin wrappers.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/controller.rs` | Replace inline retry logic with `RetryPolicy::decide` calls + a `match` on `RetryDecision` |

### Sub-step A — Identify the existing sites

Three places make retry decisions today:

1. `run_print_mode` (and its interactive counterpart) around
   `controller.rs:347–371`.
2. `ControllerState::should_retry_after_overflow` at
   `controller.rs:836`.
3. `ControllerState::should_retry_transient` at
   `controller.rs:843`.
4. `ControllerState::schedule_transient_retry` at
   `controller.rs:852` (consumes the decision to emit events).
5. `ControllerState::retry_after_overflow` at
   `controller.rs:888` (consumes `Compact`).

### Sub-step B — Replace the decision sites

At each point that currently calls
`state.should_retry_after_overflow(&result)` followed by
`state.retry_after_overflow(...)`, replace with:

```rust
let policy = RetryPolicy { config: &state.retry_config };
let decision = policy.decide(error, retry_attempt, already_compacted);
match decision {
    RetryDecision::Compact => {
        state.retry_after_overflow(&event_tx).await?;
        already_compacted = true;
        /* continue loop */
    }
    RetryDecision::Retry { attempt, delay_ms } => {
        state.schedule_transient_retry_with_delay(&event_tx, error, attempt, delay_ms).await?;
        retry_attempt = attempt;
    }
    RetryDecision::GiveUp { reason } => {
        log_give_up(reason);
        break;
    }
}
```

The helpers retain their behaviors — `retry_after_overflow` still
emits the compaction-retry system messages, `schedule_transient_
retry_with_delay` still emits `RetryScheduled` — but the decision
is no longer computed inside them.

### Sub-step C — Delete decision helpers

Once all callers use `RetryPolicy::decide`, remove:

- `ControllerState::should_retry_transient`
- `ControllerState::should_retry_after_overflow`

And rename `schedule_transient_retry(event_tx, error, attempt)` to
`schedule_transient_retry_with_delay(event_tx, error, attempt,
delay_ms)` so the delay is computed once (by `decide`) and passed
in.

### Sub-step D — Preserve `already_compacted`

The controller run loop currently does not carry an
`already_compacted: bool` explicitly — it's implicit in the
control flow. Add it as a mutable local in the relevant loop.

### Test plan

| # | Test |
|---|------|
| 1 | All existing controller tests still pass |
| 2 | New `controller_compaction_retry_path` — mock provider that returns `ContextOverflow` once then succeeds; assert compaction was triggered and the second attempt succeeded |
| 3 | New `controller_compaction_give_up_after_second_overflow` — provider returns `ContextOverflow` twice; assert we give up with `AlreadyCompacted` |
| 4 | New `controller_transient_retry_exhausts_attempts` — provider returns `Transport` forever; assert we retry `max_retries` times then give up |
| 5 | Clippy clean |

(Tests 2–4 may be integration-level in `anie-cli/src/tests.rs` or
wherever controller tests live today. Use the existing
`MockProvider`; plan 05 added typed constructors so these tests
don't need to format error strings.)

### Exit criteria

- [ ] `should_retry_transient` / `should_retry_after_overflow`
      removed.
- [ ] `RetryPolicy::decide` is the single source of retry truth.
- [ ] No test regresses.

---

## Phase 3 — Move retryability off `ProviderError`

**Goal:** Reconcile plan 05 Design Principle 2. Delete
`ProviderError::is_retryable` and
`ProviderError::retry_after_ms`; any remaining caller (there
shouldn't be, after phase 2) goes through `RetryPolicy`.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-provider/src/error.rs` | Delete `is_retryable`; delete `retry_after_ms` OR narrow it to `#[cfg(test)] pub fn`, since `retry_after_ms` is genuinely a property of the error variant |

### Sub-step A — Decide on `retry_after_ms`

Two options:

1. Keep `retry_after_ms` on `ProviderError`. Argument: `RateLimited
   { retry_after_ms }` carries the value; accessing it via a method
   is the cleanest way. Design Principle 2 was about *decisions*;
   reading a server-sent hint is not a decision.
2. Remove it. Callers destructure `ProviderError::RateLimited {
   retry_after_ms, .. }` directly. Slightly more verbose.

**Pick option 1.** It's not a decision — it's a field accessor.
Update the principle:

> Retry *decisions* are not in the error type. Retry-relevant
> *fields* (server-sent `Retry-After`) are read back via trivial
> accessors.

### Sub-step B — Delete `is_retryable`

Grep `is_retryable` across the workspace:

```
grep -rn 'is_retryable' crates/
```

Current callers (pre-plan-fix-03b):

- `controller.rs::should_retry_transient` — removed in phase 2.
- `retry_policy.rs::retry_delay_ms` — reads `retry_after_ms`, not
  `is_retryable`.

After phase 2 lands, `is_retryable` should have zero callers. If
any remain, migrate them to a `match error { ... }` or to
`RetryPolicy::decide`.

Delete the method.

### Sub-step C — Update `error.rs` doc comment

The module-level doc comment currently hints that `is_retryable`
exists; update it to name `RetryPolicy::decide` as the decision
owner.

### Test plan

| # | Test |
|---|------|
| 1 | `cargo check --workspace` passes (catches any remaining caller) |
| 2 | `grep -rn 'is_retryable' crates/` returns zero hits |
| 3 | All existing tests pass |

### Exit criteria

- [ ] `ProviderError::is_retryable` deleted.
- [ ] `ProviderError::retry_after_ms` kept and documented as a
      pure field accessor.
- [ ] Plan 05 Design Principle 2 updated in the plan doc (or a
      one-line note added to `implementation_review_2026-04-18.md`
      marking the reconciliation complete).

---

## Phase 4 — Update plan 05 doc

**Goal:** The plan 05 doc reflects the resolved shape. Future
readers don't hit the same "plan says one thing, code does
another" surprise.

### Files to change

| File | Change |
|------|--------|
| `docs/refactor_plans/05_provider_error_taxonomy.md` | Revise Design Principle 2 to match reality after phase 3 |

### Sub-step A — Wording

Change:

> 2. **Retryability is not in the error type.** It's a property
>    derived by `RetryPolicy` (plan 03, phase 4). Errors stay
>    descriptive; retry decisions stay in one place.

to:

> 2. **Retry *decisions* are not in the error type.** The decision
>    ("retry? compact? give up?") is derived by `RetryPolicy::decide`
>    in `crates/anie-cli/src/retry_policy.rs`. Retry-relevant
>    *fields* (server-sent `Retry-After` value) remain as error
>    variant data, accessed via a trivial `retry_after_ms()` method.

### Exit criteria

- [ ] Plan 05's Design Principle 2 is accurate.
- [ ] A "Status (post-fix-03b)" line is added to plan 05's header
      noting the reconciliation.

---

## Files that must NOT change

- `crates/anie-provider/src/provider.rs` — trait signatures stay.
- `crates/anie-provider/src/registry.rs` — unchanged.
- The `ProviderError` variant shape (fields and variants) — only
  the methods change.
- `crates/anie-agent/src/agent_loop.rs` retry behavior — any retry
  logic there already uses `AgentLoop`-internal machinery, not
  `ProviderError::is_retryable`.

## Dependency graph

```
Phase 1 (decide + tests)
  └── Phase 2 (migrate callers)
        └── Phase 3 (delete is_retryable)
              └── Phase 4 (doc update)
```

Strictly sequential: each phase depends on the previous.

## Out of scope

- Extension-registered retry policies (tracked in plan 10).
- Changing `RetryConfig`'s knobs (max_retries, backoff). This is
  an extraction, not a policy rewrite.
- `agent_loop.rs` internal retry handling for tool execution —
  separate concern.
- Adding a circuit-breaker ("give up this provider for the session
  after N errors"). Not in scope.
