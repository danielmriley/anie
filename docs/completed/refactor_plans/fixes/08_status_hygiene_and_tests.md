# Fix 08 — Plan 08 status hygiene + missing Phase D tests

Low-impact but cheap clean-up pass: correct the stale status
header on plan 08 and backfill the three unit tests that Phase D
specified but did not land.

## Motivation

Plan 08's status header at the top of
`docs/refactor_plans/08_small_hygiene_items.md` reads:

```
> - **Phase B (HTTP panics):** Not landed — plan 04 phase 1
>   provides the better fix ...
> - **Phase D (event send logging):** Not landed. Queued.
```

Both phases actually landed in commit `d51972b` ("Plan 08 phases B
& D: HTTP fallback + send_or_warn helper"). Reading the plan doc
without checking `git log` gives an incorrect picture of the work.

Phase D additionally specified three unit tests:

| # | Test |
|---|---|
| 1 | `send_or_warn_logs_once_when_channel_closed` |
| 2 | `send_or_warn_does_not_log_on_first_success` |
| 3 | `existing_agent_loop_tests_unaffected` |

None of the three exist. The helper's behavior (log once, silent
thereafter) is currently verified only by the integration-path
tests that happen to exercise it — there's no direct assertion on
the log emission.

## Design principles

1. **Status docs match reality.** An incorrect status field is
   worse than no status field.
2. **Cheap tests for cheap helpers.** The `send_event` helper is
   ~6 lines; the three tests are ~30 LOC combined. Pay now before
   someone edits the helper and subtly breaks the "warn once"
   semantics.
3. **No behavior change.** This plan does not modify `send_event`.

## Preconditions

- Plan 08 Phase B and D are committed.
- `send_event` exists at `crates/anie-agent/src/agent_loop.rs:23`.

---

## Phase 1 — Correct plan 08's status header

**Goal:** The status block at the top of
`08_small_hygiene_items.md` reflects the current state.

### Files to change

| File | Change |
|------|--------|
| `docs/refactor_plans/08_small_hygiene_items.md` | Rewrite the "Phase B" and "Phase D" lines in the status block; add a one-line pointer to the commit |

### Sub-step A — Proposed replacement text

Change:

```
> - **Phase B (HTTP panics):** Not landed — plan 04 phase 1
>   provides the better fix (shared `http::client() ->
>   Result<...>`). The current `#[allow(clippy::expect_used)]`
>   with justification is the interim.
```

to:

```
> - **Phase B (HTTP panics):** Landed in commit `d51972b`.
>   `local.rs::detect_local_servers` now logs and returns an
>   empty vec on client-build failure rather than panicking; the
>   shared `http::shared_http_client()` from plan 04 phase 1
>   covers the hot path. One `.expect(...)` remains in
>   `http::create_http_client()` (the cold-path fallback), gated
>   behind `#[allow(clippy::expect_used)]` with a panic
>   justification.
```

Change:

```
> - **Phase D (event send logging):** Not landed. Queued.
```

to:

```
> - **Phase D (event send logging):** Landed in commit `d51972b`.
>   The helper shipped as `send_event` (not `send_or_warn`) and
>   lives in `agent_loop.rs`. It uses a process-global
>   `AtomicBool` latch (`EVENT_DROP_WARNED`) so we warn exactly
>   once per process lifetime rather than once per channel. 60
>   call sites across `agent_loop.rs` and `controller.rs` migrated.
>   Three unit tests called out in Phase D were not landed — see
>   `fixes/08_status_hygiene_and_tests.md` for the backfill.
```

### Exit criteria

- [ ] Status block reflects commit reality.
- [ ] Forward-pointer to the test-backfill fix plan (this one)
      exists.

---

## Phase 2 — Backfill the three unit tests for `send_event`

**Goal:** The "warn once" semantics are directly asserted.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-agent/src/agent_loop.rs` | Add a `#[cfg(test)] mod send_event_tests { ... }` with three tests |
| `crates/anie-agent/Cargo.toml` | Add `tracing-test = "0.2"` as a dev-dependency (or `tracing-subscriber` if that's the existing idiom — pick whichever the workspace already uses) |

### Sub-step A — Pick the test mechanism

Two options for asserting on `tracing::warn!` emission:

1. `tracing-test` crate — provides `#[traced_test]` macro that
   installs a subscriber and lets you assert with
   `logs_contain(...)`. Minimal boilerplate.
2. Hand-rolled in-memory `Subscriber` implementation — more work,
   no new dep.

**Pick `tracing-test`** unless the workspace has a strong
no-new-dev-deps policy. Grep `Cargo.toml` files for existing uses
of `tracing-test`; if present, use the same version. If not, adopt
the current stable `0.2` series.

### Sub-step B — Test 1: `send_event_logs_once_when_channel_closed`

```rust
use tracing_test::traced_test;

#[tokio::test]
#[traced_test]
async fn send_event_logs_once_when_channel_closed() {
    let (tx, rx) = mpsc::channel::<AgentEvent>(1);
    drop(rx); // receiver gone

    // Reset the global latch so this test is self-contained.
    EVENT_DROP_WARNED.store(false, Ordering::Relaxed);

    send_event(&tx, AgentEvent::SystemMessage { text: "first".into() }).await;
    send_event(&tx, AgentEvent::SystemMessage { text: "second".into() }).await;
    send_event(&tx, AgentEvent::SystemMessage { text: "third".into() }).await;

    let log_count = logs_contain_count("agent event channel closed");
    assert_eq!(
        log_count, 1,
        "expected exactly one warn log; got {log_count}"
    );
}
```

(`logs_contain_count` is `tracing-test`'s mechanism; adjust the
exact API to the crate's current form.)

### Sub-step C — Test 2: `send_event_does_not_log_on_first_success`

```rust
#[tokio::test]
#[traced_test]
async fn send_event_does_not_log_on_first_success() {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(4);

    EVENT_DROP_WARNED.store(false, Ordering::Relaxed);

    send_event(&tx, AgentEvent::SystemMessage { text: "ok".into() }).await;

    assert!(!logs_contain("agent event channel closed"));

    // Consume to prove it actually delivered.
    let _ = rx.recv().await.expect("event should deliver");
}
```

### Sub-step D — Test 3: `send_event_latch_is_process_global`

Document the intentional process-global behavior:

```rust
#[tokio::test]
#[traced_test]
async fn send_event_latch_is_process_global() {
    // Two separate channels; both closed. The latch is shared, so
    // only the FIRST drop triggers a warn, even across channels.
    let (tx1, rx1) = mpsc::channel::<AgentEvent>(1);
    let (tx2, rx2) = mpsc::channel::<AgentEvent>(1);
    drop(rx1);
    drop(rx2);

    EVENT_DROP_WARNED.store(false, Ordering::Relaxed);

    send_event(&tx1, AgentEvent::SystemMessage { text: "a".into() }).await;
    send_event(&tx2, AgentEvent::SystemMessage { text: "b".into() }).await;

    assert_eq!(logs_contain_count("agent event channel closed"), 1);
}
```

This test pins the documented semantics: "We warn once per
process, not once per channel." If a future edit moves the latch
to per-channel, this test catches the change.

### Sub-step E — Parallel-test safety

`EVENT_DROP_WARNED` is a process-global. Rust test runners
default to running tests in parallel; tests that mutate shared
statics can race.

Options:

- Serialize with a `#[serial_test]` crate annotation.
- Run these tests under a single-threaded harness by putting them
  in their own `#[cfg(test)] mod tests` with a comment that
  parallel runs will interfere.
- Accept flakiness (no).

**Pick `serial_test`** if it's already a dev-dep; otherwise, gate
the three tests on `#[cfg_attr(test, serial_test::serial)]` and
add `serial_test = "3"` as a dev-dep. Small addition; standard
pattern for this situation.

### Sub-step F — Matching Phase D test 3

Phase D's third bullet was
`existing_agent_loop_tests_unaffected` — already satisfied by the
current state (all existing tests pass; plan 08 Phase D was
landed). Do not duplicate.

### Test plan

| # | Test |
|---|------|
| 1 | The three new tests pass |
| 2 | Running `cargo test -p anie-agent -- --test-threads=4` does not introduce flakiness (re-run 3× to confirm) |
| 3 | `cargo clippy --workspace --all-targets -- -D warnings` passes |

### Exit criteria

- [ ] Three direct tests exist for `send_event` log semantics.
- [ ] Tests are serialized if the runtime is multi-threaded.
- [ ] No new non-dev workspace dependencies.

---

## Phase 3 — Cross-reference the review doc

**Goal:** `implementation_review_2026-04-18.md`'s note about
"Plan 08 header is stale" is crossed out once this fix lands.

### Files to change

| File | Change |
|------|--------|
| `docs/refactor_plans/implementation_review_2026-04-18.md` | Under "Plan 08 — Small hygiene items", append "✅ status header corrected and Phase D tests backfilled via `fixes/08_status_hygiene_and_tests.md`" |

### Exit criteria

- [ ] Review doc has a pointer to this fix.

---

## Files that must NOT change

- `crates/anie-agent/src/agent_loop.rs` beyond the new test module
  and any trivial import reshuffle the tests need. The production
  `send_event` implementation stays as-is.
- Any other crate's `Cargo.toml` — the dev-dep lives in
  `anie-agent` only.

## Dependency graph

```
Phase 1 (status fix) ──┐
Phase 2 (tests)        ──┼── Phase 3 (review cross-reference)
```

All three can ship as one PR. Split only if reviewer prefers.

## Out of scope

- Adding tests for plan 08 Phase B's HTTP fallback behavior.
  `detect_local_servers` with a failed client is hard to simulate
  without mocking `reqwest::Client::builder`, and the behavior
  (warn + return empty vec) is simple enough that the code
  review is the review.
- Replacing `EVENT_DROP_WARNED` with a per-`AgentLoop` or
  per-channel latch. That's a semantics change, not a test
  backfill.
- Migrating other `let _ = ...send(...)` sites beyond `agent_loop`
  and `controller`. The five remaining sites in other crates
  (`anie-tools/src/bash.rs`, `anie-tui/src/app.rs`,
  `controller.rs` UI-action forwarders) are intentional — they
  send to UI-side channels that may already have dropped during
  shutdown. If they deserve migration, that's a separate plan.
