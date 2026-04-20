# Plan 13 — Controller responsiveness + reliable action delivery

Two coupled merge-blockers from the followup review: the controller
stalls during retry backoff, and high-value UI actions can be
silently dropped by `try_send` on a bounded channel. Together they
make the TUI feel frozen during retries and, worse, can lose user
input without any surface-level indication.

## Motivation

### Blocker 1: retry backoff blocks the controller task

`crates/anie-cli/src/controller.rs:668–690` currently implements
transient-retry backoff inline:

```rust
async fn schedule_transient_retry_with_delay(...) -> Result<()> {
    ...
    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    Ok(())
}
```

This is awaited from the retry path inside the controller's main
`loop`. During that sleep (up to `max_delay_ms = 30_000`), the
controller is not polling `ui_action_rx.recv()`. Consequences:

- **Ctrl+C during retry backoff feels ignored** — the abort arrives
  in the channel but isn't drained until the sleep returns.
- **Quit is delayed** for the same reason.
- Print-mode and RPC-mode aborts (same shutdown signal forwarder)
  are similarly delayed.

The retry policy's exponential backoff can accumulate — with
`backoff_multiplier: 2.0` and three attempts, total sleep is up to
`1s + 2s + 4s = 7s` of unresponsiveness in the worst case, longer
if `Retry-After` headers extend it.

### Blocker 2: TUI silently drops actions on a full channel

The TUI → controller channel:

- Constructed as `mpsc::channel(64)` in
  `crates/anie-cli/src/interactive_mode.rs:26`.
- Every outbound action uses `action_tx.try_send(...)` with the
  `Err` dropped: `let _ = self.action_tx.try_send(...)` appears at
  ~15 sites in `anie-tui/src/app.rs`.

On a full channel:

- `SubmitPrompt` (user hits Enter) is dropped. Input clears, so the
  user sees nothing happen.
- `Quit` / `Abort` are dropped. Ctrl+C twice may not close the app.
- `/thinking`, `/model`, `/reload` are dropped. The system message
  the user was expecting never arrives.

The channel only fills when the controller is slow to drain — and
blocker 1 is exactly that scenario. The two bugs reinforce each
other.

### Why both in one plan

Fixing either in isolation still leaves a bad state:

- Fix backoff only → retry becomes responsive, but a busy
  controller (e.g. compaction, tool loop) can still drop actions
  under load.
- Fix channel only → actions arrive reliably, but retry backoff
  still feels frozen.

They share an invariant — *user actions reach the controller
promptly* — and the test for that invariant (submit a prompt during
backoff and assert it is processed) is the same regardless of which
fix we make first.

## Scope

Two phases, both in `anie-cli` + `anie-tui`. No changes to
providers, protocol, session, or tools crates. No behavior change
for success paths — existing tests must stay green untouched.

## Design principles

1. **The controller's main loop is a select over everything.** No
   awaited sleep inside a branch. State machines instead of inline
   timers.
2. **User actions are not lossy.** Submit, Quit, Abort, SetModel,
   SetThinking, SwitchSession, ForkSession, NewSession, ReloadConfig,
   Compact, ShowDiff, ShowTools, ShowHelp, GetState, SelectModel,
   ListSessions — all of these represent a deliberate user
   decision. The pipe that carries them must not silently drop.
3. **Display-side side effects stay local.** `ClearOutput` is the
   only action that both the TUI and the controller act on (the
   TUI clears its output pane locally; the controller no-ops). It
   is safe to lose if the pane was already cleared — but there's
   no good reason to make it lossy either.
4. **Unbounded action channel, bounded event channel.** The TUI is
   the producer of actions at human-typing rate; the controller is
   the producer of events at LLM-streaming rate. Unbounded up, but
   keep events bounded so backpressure remains a real signal on
   the stream path.
5. **Cancellation survives state transitions.** A pending retry
   must respect the user cancel token. If the user aborts during
   the backoff, we do not start the continuation run.

---

## Phase A — Non-blocking retry backoff

**Goal:** The controller polls `ui_action_rx` continuously during
retry backoff. Ctrl+C and other actions arrive within one event-
loop tick.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/controller.rs` | Replace the inline `tokio::time::sleep` with a pending-retry state tracked on `InteractiveController`. Expand the main-loop `select!` with a `sleep_until(next_retry_at)` branch. |
| `crates/anie-cli/src/controller_tests.rs` | Test: enqueue a retryable error, advance paused time, assert a `UiAction::Abort` dispatched during the backoff is observed before the continuation run starts (i.e. no continuation run is ever started when the user aborts during backoff). |

### Sub-step A — State representation

Introduce a new field on `InteractiveController`:

```rust
enum PendingRetry {
    None,
    /// A transient retry is armed. The timer is due at
    /// `deadline`; on fire the controller starts a continuation
    /// run with the captured attempt/compacted state.
    Armed {
        deadline: Instant,
        attempt: u32,
        already_compacted: bool,
    },
}
```

The current `CurrentRun` already carries `retry_attempt` and
`already_compacted`; `PendingRetry::Armed` is the *between-runs*
version of that state.

### Sub-step B — Main loop: one `select!`

The main loop currently branches on `current_run.is_some()`. Extend
that to a three-way state: `Run(_)`, `PendingRetry::Armed(_)`, or
`Idle`. Each arm selects over:

- `ui_action_rx.recv()` — always polled.
- The state-specific future:
  - `Run`: `&mut current_run.handle` + `current_run.cancel.cancelled()`
  - `PendingRetry::Armed`: `tokio::time::sleep_until(deadline)`
  - `Idle`: no extra future (just UI actions and quit).

On timer fire while `PendingRetry::Armed`:

- Clear the pending state.
- Call `start_continuation_run(already_compacted, attempt)`.

On `UiAction::Abort` while `PendingRetry::Armed`:

- Clear the pending state. Emit a system message
  `"Retry aborted by user."` No continuation run is started.

On `UiAction::Quit` while `PendingRetry::Armed`:

- Clear the pending state, set `self.quitting = true`, break.

### Sub-step C — Retract the awaited sleep

`schedule_transient_retry_with_delay` currently both emits the
`RetryScheduled` event **and** sleeps. Split it:

```rust
async fn emit_retry_scheduled(
    &self,
    error: &ProviderError,
    attempt: u32,
    delay_ms: u64,
) { /* send event, no sleep */ }
```

The sleep is deleted. The retry decision path now records
`PendingRetry::Armed` and continues to the next loop iteration.

### Sub-step D — Cancel-during-backoff semantics

Add a helper `abort_pending_retry(&mut self, reason: &str)` that
clears `PendingRetry::Armed` and surfaces the reason as a system
message. Called from the Abort and Quit arms.

### Test plan

| # | Test (in `controller_tests.rs`) |
|---|---|
| 1 | `retry_backoff_polls_ui_actions` — Script a retryable provider error. Before the retry timer fires, send `UiAction::GetState` and assert the controller emits the status system message. Use `tokio::time::pause()` + `advance()` for deterministic timing. |
| 2 | `abort_during_retry_backoff_cancels_retry` — Retryable error → retry armed → `UiAction::Abort`. Assert the pending retry is cleared and no continuation run starts, even after advancing time past the deadline. |
| 3 | `quit_during_retry_backoff_exits_cleanly` — Same setup, `UiAction::Quit`. Assert the controller loop exits without panicking, no continuation spawn. |
| 4 | `retry_fires_continuation_when_deadline_elapses` — Retryable error → retry armed → advance time → assert `AgentStart` from the continuation run appears. |
| 5 | Existing `controller_compaction_retry_path` et al. still pass (no changes to semantics, just to scheduling). |

### Exit criteria

- [ ] No `tokio::time::sleep(Duration::from_millis(delay_ms))` calls
      inside `controller.rs`. All timing flows through
      `sleep_until`.
- [ ] Main loop has one `select!` per state, each polling
      `ui_action_rx`.
- [ ] Tests 1–4 pass. Existing retry tests pass untouched.
- [ ] Manual smoke: kick a provider to transient-retry; Ctrl+C
      during backoff exits within one second.

---

## Phase B — Reliable TUI action delivery

**Goal:** A user action submitted while the controller is busy
still reaches the controller. No silent drops.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/interactive_mode.rs` | Switch `mpsc::channel(64)` to `mpsc::unbounded_channel()` for `UiAction`. |
| `crates/anie-tui/src/app.rs` | Replace `action_tx: mpsc::Sender<UiAction>` with `mpsc::UnboundedSender<UiAction>`. Replace every `try_send` with `send()` (unbounded `send` is synchronous). Surface send failures (`SendError` → channel closed) as a visible system message. |
| `crates/anie-cli/src/controller.rs` | Accept `mpsc::UnboundedReceiver<UiAction>`; update `recv()` call sites (API is identical). |
| `crates/anie-cli/src/print_mode.rs`, `crates/anie-cli/src/rpc.rs` | Mirror the type change at channel-construction sites. |
| `crates/anie-cli/src/bootstrap.rs` | `spawn_shutdown_signal_forwarder` currently holds a `mpsc::Sender<UiAction>`. Swap to `UnboundedSender` and use `send()`. |
| `crates/anie-tui/src/tests.rs`, `crates/anie-cli/src/controller_tests.rs` | Update test setup to use `mpsc::unbounded_channel()`. |
| `crates/anie-integration-tests/tests/agent_tui.rs` | Same type update. |

### Sub-step A — Why unbounded for actions

User actions arrive at human-typing speed (worst case ~10/sec for
rapid typing). The controller drains them at microsecond
granularity when idle. The channel is bounded today purely as a
defensive measure; there is no real memory concern with an
unbounded queue for this traffic shape.

Unbounded also has a second property the bounded channel lacks:
`UnboundedSender::send()` is synchronous and never blocks or
yields. That means the TUI key handler — which is running inside
the main render loop — cannot stall the UI by awaiting capacity.
A bounded channel with `send().await` would have the opposite
problem: when the controller is slow, the TUI itself freezes.

Events (controller → TUI) stay bounded (256). Those arrive at
streaming-token rate; backpressure on that path is a real signal
and the current implementation handles it correctly.

### Sub-step B — Classify callsites and handle send errors

In `app.rs`, every call site has the shape:

```rust
let _ = self.action_tx.try_send(UiAction::Foo);
```

After this change, the only failure mode is `SendError` (receiver
dropped — process is shutting down). Classify:

1. **Action during shutdown** — the controller has exited and the
   receiver is gone. Nothing to do; the ongoing quit will finish.
   Silently ignoring is correct here.
2. **Any other send failure** — not reachable for unbounded
   channels except via closed-receiver. Treat identically to #1.

So the mechanical change is:

```rust
if self.action_tx.send(UiAction::Foo).is_err() {
    // receiver has been dropped — process is tearing down.
    // No user-facing message needed; the quit flow will finish.
}
```

Most existing `let _ =` drops are fine as-is. The audit adds a one-
line system-message on send failure **only** for actions where the
user expects visible feedback — principally `SubmitPrompt` —
because the previous failure mode (channel full) was indistinguishable
from the new one (receiver dropped), and the user still wants to
know "your prompt didn't go through" when things break.

### Sub-step C — The `bootstrap::spawn_shutdown_signal_forwarder` seam

This helper forwards Ctrl+C / SIGTERM to the controller as
`UiAction::Quit`. Today it holds a `mpsc::Sender<UiAction>`. Swap
to `UnboundedSender<UiAction>`; the API change is mechanical.

### Sub-step D — Integration-test plumbing

`anie-integration-tests/tests/agent_tui.rs:16` constructs a dummy
`(action_tx, _action_rx) = mpsc::channel(8)`. Update to
`unbounded_channel()`. No semantic change.

### Test plan

| # | Test (in `anie-tui/src/tests.rs` or `controller_tests.rs`) |
|---|---|
| 1 | `action_channel_does_not_drop_under_burst` — In a TUI test, fill the channel with 1000 `ClearOutput` actions back-to-back. Receiver drains them all. Contrast this with a regression-mode check that `mpsc::channel(64).try_send` would have dropped ~936. |
| 2 | `submit_prompt_reaches_controller_while_run_is_active` — Start the controller, begin a mock long-running run, submit a second prompt. Assert the controller emits the "run already active" system message (proving the action arrived even when busy). Today this test would flake if the channel filled; after the fix it is deterministic. |
| 3 | `send_failure_after_controller_exit_is_silent` — Drop the receiver, then send an action. Must not panic. No system message required. |
| 4 | The existing Ctrl+C tests in `anie-tui/src/tests.rs` still pass untouched. |

### Exit criteria

- [ ] `cargo grep` for `mpsc::channel.*UiAction` returns zero
      matches in production code.
- [ ] All `try_send(UiAction::...)` replaced with `send(...)`.
- [ ] `UiAction` receiver is `UnboundedReceiver<UiAction>`
      throughout.
- [ ] New tests 1–3 pass.
- [ ] Full workspace `cargo test` green; clippy clean.

---

## Phase ordering

A → B. Phase A is strictly smaller; it's the one that resolves the
observable user symptom (Ctrl+C feels dead during retry). Phase B
is the durable fix that eliminates a whole class of latent bugs
and makes the new "polls during backoff" behavior actually usable
under load.

## Risks

1. **Time manipulation in tests.** Phase A's tests depend on
   `tokio::time::pause`. We already use `tokio::time::sleep` in
   the code path being tested, so the pattern is familiar. Run the
   affected tests under both paused and real-time at least once
   during implementation to confirm no race.
2. **Unbounded memory growth.** Theoretically possible if a
   component produces actions faster than the controller drains
   them. The TUI is rate-limited by user keystrokes (worst case
   ~10/s). Memory is bounded by the process lifetime. Not a real
   risk.
3. **Integration with existing retry-count tests.** The three
   retry tests in `controller_tests.rs` assert on the number of
   retries. They already use `initial_delay_ms: 1` /
   `max_delay_ms: 1` / `jitter: false` to keep total sleep
   negligible. After Phase A they become slightly faster (no
   sleep at all in the test path) — no behavioral change.

## Out of scope

- Redesigning the action enum or splitting into critical/lossy
  groups. All actions keep their current shape.
- Event channel changes (controller → TUI). Those remain bounded;
  backpressure on streaming events is the correct behavior.
- Retry-after-header handling changes. Same fields, just
  non-blocking delivery.
