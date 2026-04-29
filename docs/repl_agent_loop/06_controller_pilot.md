# PR 6 — Controller pilot integration

**Goal:** Route the controller's spawned agent task through
`AgentRunMachine` instead of `AgentLoop::run`, while preserving
the controller's existing run-result contract. This proves the
step machine works end-to-end in production and creates the
seam PR 7's policy hook plugs into.

This PR is behavior-preserving from the user's perspective.

## Rationale

After PR 5, `AgentLoop::run` is a thin wrapper over
`AgentRunMachine`. The controller currently spawns
`AgentLoop::run` at `crates/anie-cli/src/controller.rs:1003-1006`
and consumes the resulting `AgentRunResult` in the main `select!`
arm at `controller.rs:262-413`. The retry / compaction / queued-
prompt policy at `controller.rs:265-339` runs *after* the run
returns.

PR 6 inverts that one layer: instead of spawning `AgentLoop::run`
and waiting for one `AgentRunResult`, the spawned task drives
the step machine to completion itself and returns the same
`AgentRunResult`. From the controller's perspective, nothing
changes — same spawn, same await, same retry policy, same
session persistence.

The point is to verify the machine works in the real interactive
path *before* PR 7 starts using it for policy interposition.

## Design

### Two-step approach

**6A: Switch the spawn target** (this is the whole PR for the
default case).

Change `controller.rs:1003-1006` from:

```rust
let task = tokio::spawn(async move {
    agent.run(vec![prompt_message], context, event_tx, task_cancel).await
});
```

to:

```rust
let task = tokio::spawn(async move {
    let mut machine = agent.start_run_machine(
        vec![prompt_message],
        context,
        &event_tx,
    );
    while !machine.is_finished() {
        machine.next_step(&event_tx, &task_cancel).await;
    }
    machine.finish()
});
```

That's it. The result type is still `AgentRunResult`. The
controller's `select!` arm still consumes `run_result` the same
way. The retry / compaction / queue policy at
`controller.rs:265-339` is untouched.

> If `start_run_machine` ends up needing the sender by value
> (rather than reference) to manage lifetimes inside the spawned
> task, take it by value here and clone the controller's
> `event_tx` before the move. The clone is cheap.

**6B: Apply the same change to print mode and any other
`AgentLoop::run` callsite.**

Find every `AgentLoop::run` call in the workspace:

```bash
rg "AgentLoop::run\b|agent\.run\b|\.run\(.*event_tx" crates/
```

Likely callsites:

- `crates/anie-cli/src/controller.rs:1003` (interactive,
  covered by 6A).
- Print mode (the non-interactive entry point — likely in
  `crates/anie-cli/src/main.rs` or a sibling).
- Any RPC mode entry point if one exists.
- Integration tests in `crates/anie-integration-tests/` that
  drive the agent end-to-end.

Each gets the same treatment: spawn or call the machine driver
instead of `AgentLoop::run`.

> Alternative: leave `AgentLoop::run` in place as the
> recommended default and keep all existing callers on it. The
> machine is already a strict subset of `run`, so the only PR
> that needs to switch is the controller's spawn — that's the
> seam PR 7 needs. Pick the alternative if 6B turns up many
> callsites and the diff balloons. The whole point of PR 5's
> wrapper is that callers can stay simple.

**Recommendation: 6A only for this PR.** Defer 6B until a
real consumer (e.g., a future PR that wants step-level policy
in print mode) needs the seam there.

### Architecture doc update

This PR is the right time to update
`docs/arch/anie-rs_architecture.md` to describe the REPL loop
as the current architecture. Two paragraphs:

1. The agent loop is a Read → Eval → Print → Decide REPL
   driver, exposed publicly as `AgentRunMachine`. Each step
   evaluates one bounded intent (model turn, tool batch,
   follow-up append) and produces one observation.
2. The controller spawns the machine as a single task and
   consumes one `AgentRunResult` per run. Run-level policy
   (retry, compaction, session persistence, queued prompts)
   stays in the controller. Step-level policy hooks plug in at
   PR 7's boundary (forward-reference).

## Files to touch

- `crates/anie-cli/src/controller.rs` — change the spawn target
  at line ~1003. ~10 LOC diff.
- `docs/arch/anie-rs_architecture.md` — update the
  agent-loop section. ~20 LOC diff.

If 6B is included:

- `crates/anie-cli/src/main.rs` (or wherever print mode lives).
- Any other callsite of `AgentLoop::run`.

## Test plan

The controller already has tests in `anie-cli`'s tests. After
this PR they should pass unchanged — that's the contract:

- `cargo test -p anie-cli`.
- `cargo test --workspace`.
- `cargo clippy --workspace --all-targets -- -D warnings`.

Beyond that, the manual smoke is the most important check —
this is the first PR where a real LLM stream goes through the
machine. Use
`.claude/skills/live-provider-smoke/SKILL.md` as guidance:

- One OpenRouter free-tier prompt without tools — assert
  streaming feels live, no perceptible lag, full response
  arrives.
- One prompt with at least one tool call — assert the tool
  fires, results stream, follow-up assistant streams.
- One prompt cancelled mid-stream with Ctrl+C — assert
  cancellation reaches the provider and the run terminates
  cleanly.
- One forced compaction scenario (long context) if practical
  — assert the controller's compaction-retry path still fires
  through the new spawn shape.

## Risks

- **The spawned task's lifetime ergonomics differ subtly from
  `AgentLoop::run`.** Mitigation: keep the spawn body small;
  the machine driver loop is 4 lines. If `event_tx` ownership
  becomes awkward inside the closure, take it by value.
- **A subtle difference in start-event ordering breaks the
  TUI.** Mitigation: PR 1's tests cover this through
  `AgentLoop::run` (which still wraps the machine). Add one
  controller-level test if needed: spawn the machine driver
  exactly as the controller does, drain events, assert they
  match the order produced by `AgentLoop::run`.
- **PR 6 unmasks a latent bug from PRs 2–5 that didn't show up
  in unit tests.** Mitigation: that's the whole reason this PR
  exists. The smoke check is mandatory.
- **The architecture doc update drifts from the code.**
  Mitigation: write the doc update as part of the same PR; do
  not split it into a follow-up.

## Exit criteria

- [ ] `controller.rs`'s spawn at ~line 1003 drives
      `AgentRunMachine::next_step` instead of calling
      `AgentLoop::run`.
- [ ] All `anie-cli` tests pass unchanged.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] Manual smoke (per `.claude/skills/live-provider-smoke/`)
      passes for: no-tool prompt, tool-call prompt, cancelled
      prompt.
- [ ] `docs/arch/anie-rs_architecture.md` describes the REPL
      loop as the current architecture.
- [ ] `AgentRunResult` shape unchanged.
- [ ] No `AgentEvent` variants added.

## Deferred

- 6B (other callsites). Land if a follow-up actually needs the
  seam there.
- Step-level retry. Today retry happens after `AgentRunResult`;
  moving it inside the machine is a future PR with its own
  plan.
- Step-level compaction. Same — controller-owned today, can
  move to before-model boundary in a future PR.
- Folding queued prompts into a step's `Read` phase. The
  active-input plan and PR 7 together create the seam; the
  fold itself is later.
- Surfacing step boundaries through the UI. Out of scope.
