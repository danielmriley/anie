# PR 5 — Public step-machine API

**Goal:** Expose the REPL driver as a public `AgentRunMachine`
type with `next_step` / `is_finished` / `finish` so callers can
optionally drive the loop one step at a time. `AgentLoop::run`
becomes a thin wrapper that drives the machine to completion.

This PR is additive. Existing callers do not change.

## Rationale

After PRs 2–4, `AgentLoop::run`'s body is a clean Read → Eval →
Print → Decide driver, but the only way to invoke it is to call
`run` and wait for `AgentRunResult`. PR 5 lets the controller
(eventually) interpose policy at step boundaries — for example,
folding queued user prompts into the next `Read`, running a
verifier between steps, or applying step-level retry — without
moving that policy into `anie-agent`.

The architecture doc warns against rushing this:

> Public step-machine APIs are second-stage. The basic definition
> of done is an internal REPL-shaped `AgentLoop::run`. Exposing a
> controller-driven stepper is valuable, but it should come only
> after the internal shape is stable and tested.

PRs 2–4 are the "internal shape is stable and tested" milestone.
PR 5 is the public surface.

## Design

### Public types

```rust
// crates/anie-agent/src/agent_loop.rs (or new submodule)

pub struct AgentRunMachine {
    inner: AgentRunMachineInner,  // owns AgentLoop refs, state, intent
}

pub enum AgentStepBoundary {
    /// The step ran. Inspect the machine for state if needed,
    /// then call `next_step` again.
    Continue,
    /// The run is finished. Call `finish()` to consume the
    /// machine and get the `AgentRunResult`.
    Finished,
}

impl AgentRunMachine {
    /// Drive one REPL iteration: Read → Eval → Print → Decide.
    /// Emits live streaming events through `event_tx` exactly
    /// as `AgentLoop::run` does today.
    pub async fn next_step(
        &mut self,
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> AgentStepBoundary;

    /// True iff the next call to `next_step` would return
    /// `Finished` immediately. Implemented as
    /// `state.finished`.
    pub fn is_finished(&self) -> bool;

    /// Consume the machine and produce the run result.
    /// Panics or returns an error if called before
    /// `is_finished()` is true. (Pick one — see "Ownership
    /// pattern" below.)
    pub fn finish(self) -> AgentRunResult;
}
```

### Ownership pattern: pick one

The architecture doc explicitly flags this: *"Pick one ownership
pattern for finish results: either `next_step` returns the final
`AgentRunResult`, or `next_step` reports `Finished` and a
separate consuming `finish()` returns the result. Avoid exposing
two competing ways to retrieve the same final state."*

**Recommendation: option B (`finish()` consumes).** Reasoning:

- Lets the caller inspect machine state *between* the last step
  and the result if needed (e.g., to run a verifier on the
  final assistant before officially finishing).
- Avoids the awkward `Continue { result: Option<_> }` shape.
- Mirrors the `tokio::task::JoinHandle` and `tokio_util` patterns
  the project already uses.

**Behavior of `finish()` when called early:** return
`AgentRunResult` with whatever `state` has so far, rather than
panic. Document that this gives a partial result (no
`AgentEnd` event emitted yet) and is intended for cancellation
cleanup paths. This avoids API hazards.

### Constructor

```rust
impl AgentLoop {
    pub fn start_run_machine(
        &self,
        prompts: Vec<Message>,
        context: Vec<Message>,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) -> AgentRunMachine;
}
```

`start_run_machine` does what `start_run` did internally in PR 3:

- Construct `AgentRunState::new(prompts, context)`.
- Emit `AgentStart`.
- Emit initial `TurnStart`.
- Emit prompt `MessageStart`/`MessageEnd` events.
- Set `intent = AgentIntent::ModelTurn`.

So the first call to `next_step` does exactly one REPL
iteration, just like the first iteration of today's loop.

> The constructor takes `&event_tx` because run-start emission
> must happen synchronously before the first `next_step`. An
> alternative is to emit start events lazily on the first
> `next_step`; that's cleaner but changes the lifecycle
> contract (`AgentStart` no longer happens before `next_step`
> returns). Stick with the eager pattern for PR 5.

### `AgentLoop::run` after PR 5

```rust
pub async fn run(
    &self,
    prompts: Vec<Message>,
    context: Vec<Message>,
    event_tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
) -> AgentRunResult {
    let mut machine = self.start_run_machine(prompts, context, &event_tx);
    while !machine.is_finished() {
        machine.next_step(&event_tx, &cancel).await;
    }
    machine.finish()
}
```

Five lines. The body of the loop now lives in
`AgentRunMachine::next_step`.

### Cancellation semantics

- The `CancellationToken` is *passed* to `next_step`, not owned
  by the machine. The caller can pass a different token each
  call (e.g., a per-step token derived from a per-run token).
- The default — `AgentLoop::run` — passes the same token every
  time, so behavior is unchanged.
- `next_step` checks the token at phase boundaries (between
  Read and Eval, between Eval and Print, between Print and
  Decide). Cancellation mid-Eval is handled inside `Eval`'s
  existing `tokio::select!` (currently in `collect_stream`).

### State exposure

PR 5 does **not** expose the machine's internal state
(`AgentRunState`, current `AgentIntent`, generated messages so
far) to callers. Adding read-only accessors is fine if a use
case appears, but speculative accessors get deferred. The
`finish()` result is the only state contract.

## Files to touch

- `crates/anie-agent/src/agent_loop.rs` — extract the loop body
  into `AgentRunMachine::next_step`, add public types, rewrite
  `AgentLoop::run` as the wrapper.
- `crates/anie-agent/src/lib.rs` — re-export `AgentRunMachine`
  and `AgentStepBoundary` if `agent_loop`'s public surface is
  re-exported there (check current `lib.rs` first).

Estimated diff: ~100 LOC moved from `run` into `next_step`,
~30 LOC of new public type + impl, ~5 LOC for `run` wrapper.

## Test plan

PR 1's 14 characterization tests must still pass — they exercise
`AgentLoop::run`, which now goes through the machine. If any
test breaks, the wrapper drove the machine differently from how
the original loop ran.

Add these new tests in
`crates/anie-agent/tests/agent_loop_step_machine.rs`:

| # | Test | Asserts |
|---|------|---------|
| 1 | `step_machine_one_step_at_a_time_matches_run_to_completion` | Drive a fixed scenario through `next_step` calls; drive the same scenario through `AgentLoop::run`; assert the resulting `AgentRunResult` is byte-equal (or field-equal) and the captured events are in the same order. |
| 2 | `step_machine_emits_run_start_events_before_first_step` | After `start_run_machine` returns, drain the event channel and assert `AgentStart`, `TurnStart`, prompt `MessageStart`/`MessageEnd` are present *before* any `next_step` call. |
| 3 | `step_machine_is_finished_after_terminal_observation` | Run a scenario where the assistant has no tool calls; after one `next_step`, `is_finished()` is `true`. |
| 4 | `step_machine_finish_called_early_returns_partial_result` | Cancel mid-stream; call `finish()` without further `next_step`; result has the partial assistant and `terminal_error` populated as expected. |
| 5 | `step_machine_passes_through_cancellation_per_step` | Call `next_step` with a fresh cancelled token; the call returns quickly with `Finished`; subsequent `next_step` is a no-op returning `Finished`. |

Plus:

- `cargo test --workspace`.
- `cargo clippy --workspace --all-targets -- -D warnings`.

## Risks

- **The wrapper's loop-to-completion semantics drift from the
  original `run`.** Mitigation: PR 1's tests are precisely this
  check. Run them after PR 5; they're the contract.
- **`finish()` called early is a footgun.** Mitigation: document
  the partial-result behavior in the doc comment; the test #4
  asserts it. If footguns become a concern in practice, switch
  to `Result<AgentRunResult, NotFinishedError>` in a follow-up.
- **Holding `AgentRunMachine` across `.await` requires `Send`.**
  Mitigation: ensure `AgentRunMachineInner`'s fields are `Send`;
  the existing `AgentLoop::run` is already `Send`-bounded so
  this should hold.
- **`start_run_machine` taking `&event_tx` constrains
  ergonomics.** Mitigation: take `&mpsc::Sender<AgentEvent>`
  by reference and clone only when actually needed; the sender
  is cheap to clone if it does come up.

## Exit criteria

- [ ] `AgentRunMachine` and `AgentStepBoundary` are public.
- [ ] `AgentLoop::start_run_machine` is public and emits the
      same run-start events as the current `AgentLoop::run`
      does in its first 5 lines.
- [ ] `AgentLoop::run` is now ≤10 lines and delegates to the
      machine.
- [ ] PR 1's 14 characterization tests pass unchanged.
- [ ] PR 5's 5 new step-machine tests pass.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] `AgentRunResult` shape unchanged.
- [ ] No `AgentEvent` variants added.

## Deferred

- Read-only accessors on `AgentRunMachine` (e.g.,
  `current_intent()`, `step_index()`). Add when a real consumer
  needs them.
- A `step_with` method that takes a per-step policy closure.
  PR 7 introduces a single hook; full closure-based policy is
  speculative.
- Concurrent ownership of the machine. The machine is
  `&mut self`-owned; one driver at a time. Don't add `Arc<Mutex>`
  patterns until something actually needs them.
- Replacing `AgentLoop::run` with `AgentRunMachine`. PR 5
  intentionally keeps the wrapper for callers that want the
  simple shape (print mode, RPC mode, integration tests). They
  do not need to migrate.
