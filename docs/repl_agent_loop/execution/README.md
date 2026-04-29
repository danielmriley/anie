# REPL agent-loop — execution

Tracker for the seven PRs. Update status inline as each lands.

## Baseline

`AgentLoop::run` is the current monolithic loop body
(`crates/anie-agent/src/agent_loop.rs:433-712`). The controller
spawns it at `crates/anie-cli/src/controller.rs:1003-1006` and
consumes one `AgentRunResult` per run. These PRs stack on
`main`.

| PR | Plan | Status | Commit |
|----|------|--------|--------|
| PR 1 | [Behavior characterization tests](../01_behavior_characterization.md) | landed | `2b3f951` |
| PR 2 | [Extract `AgentRunState`](../02_run_state_extraction.md) | landed | `02cc0cd` |
| PR 3 | [Internal REPL driver](../03_internal_repl_driver.md) | landed | `f053013` |
| PR 4 | [Step tracing spans](../04_step_tracing.md) | landed | `df07082` |
| PR 5 | [Public step-machine API](../05_step_machine_api.md) | landed | `f3e3cf7` |
| PR 6 | [Controller pilot integration](../06_controller_pilot.md) | not started | — |
| PR 7 | [First policy boundary: before-model](../07_first_policy_boundary.md) | not started | — |

## Ordering rationale

PRs 1–6 are strictly sequential. Each later PR depends on the
shape produced by the previous one:

- PR 1 (tests) is the contract. PRs 2–6 prove themselves
  against it.
- PR 2 (state extraction) creates the helpers PR 3 needs.
- PR 3 (REPL driver) creates the phases PR 4 instruments.
- PR 4 (tracing) is independent of PR 5 in principle but
  cheap to land in PR 4's slot — observability before public
  API exposure means we have logs when PR 5 first goes live.
- PR 5 (public step machine) creates the seam PR 6 uses.
- PR 6 (controller pilot) puts the machine on the production
  path.
- PR 7 (policy boundary) is the first capability extension
  and should not land before PR 6 is stable.

PR 4 and PR 5 could in principle reorder (instrument first vs
expose API first); the recommendation is to keep tracing first
because it gives operators visibility from the moment the
machine ships, even before any caller migrates to the public
step API.

## Gate per PR

Each PR is approved to land only when:

- [ ] All exit criteria in the plan are checked.
- [ ] `cargo test --workspace` is green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      is clean.
- [ ] PR 1's 14 characterization tests pass (after PR 1 itself
      lands).
- [ ] No public-API change unless the plan explicitly authorizes
      one (only PR 5 does).
- [ ] No new `AgentEvent` variants.
- [ ] Manual smoke per the plan's test-plan section.

## Pause points

- **After PR 1.** Confirm tests cover the behaviors the team
  cares about. Add tests if anything important is missing
  before refactoring against an incomplete contract.
- **After PR 3.** Pause to assess: does the driver shape feel
  right? Is there friction we'd want to fix before exposing it
  publicly in PR 5? This is the last cheap moment to adjust
  the internal shape before consumers care.
- **After PR 6.** Pause to assess: is there a clear first real
  consumer for PR 7's policy hook? If not, PR 7 can wait — it's
  not load-bearing on its own.

## Out of scope (post-series)

- Recursive task decomposition.
- Verifier / critic loops.
- Tool-call repair loop.
- Model capability profiles.
- Prompt-template overhauls.
- Context retrieval / repo map.
- Reflexion-style memory.
- Local-backend constrained decoding.

These are tracked in `docs/local_small_model_harness_ideas.md`
and `docs/repl_agent_loop_2026-04-27.md`. Each gets its own
plan folder once a real consumer materializes after PR 7.
