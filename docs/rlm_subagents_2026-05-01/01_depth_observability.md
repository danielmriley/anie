# PR 1 — Depth tracking for recurse calls (observability)

## Rationale

Today's `RecurseTool` already plumbs an
`Arc<AtomicU32>` recursion budget through to sub-
agents (see `build_rlm_extras` in `controller.rs`),
but it's a single counter — we know "how many recurse
calls have happened" but not "how deep is *this*
call". Once sub-agents inherit tools (PR 2) and can
call recurse themselves (PR 4), depth becomes the
relevant metric for diagnosing runaway behavior.

Per the series principle: track, log, surface — but
don't refuse. Hard caps come later only if smoke shows
observability is insufficient.

## Design

Extend the recurse-tool plumbing to carry an explicit
`current_depth: u32` value alongside the existing
budget counter. The depth field is set by the parent
when constructing the sub-agent's `SubAgentBuildContext`
(crate `anie-agent`, `recurse.rs`).

When a sub-agent constructs ITS own `RecurseTool` (in
PR 2 once tool inheritance lands), it reads
`ctx.depth` and passes `depth + 1` into its sub-sub-
agents.

Observability:

- Every `RecurseTool::execute` logs at info-level:
  `recurse: depth=<n> scope=<kind> args=<digest>`.
- Status-bar segment `depth: <max_seen>` shows the
  highest depth observed in the current session
  (cleared on session reset).
- A new ledger entry on every recurse fire:
  `recurse depth=<n>` so future-turn ledger inspection
  sees the depth pattern.

Threshold for warning (logged + status-bar
highlighted, NOT enforced): `ANIE_RECURSE_DEPTH_WARN_AT`
(default `5`). Crossing the threshold emits an
`info!` log and a `SystemMessage` event for the
transcript: *"recurse depth exceeded 5; investigate
whether sub-agents are decomposing productively or
forking unnecessarily."*

## Files to touch

- `crates/anie-agent/src/recurse.rs` — extend
  `SubAgentBuildContext` with `depth: u32`. Update
  the `SubAgentFactory` trait if needed.
- `crates/anie-cli/src/recurse_provider.rs` /
  `recurse_factory.rs` — propagate depth through
  `ControllerSubAgentFactory::build`.
- `crates/anie-tools/src/recurse.rs` — `RecurseTool`
  reads its construction-time depth and logs it on
  every fire. (Note: today the tool doesn't carry a
  depth; we'll add it.)
- `crates/anie-cli/src/controller.rs` — `build_rlm_extras`
  wires `depth = 0` for top-level. Status-bar
  composition reads max-seen depth.
- Tests in `anie-tools` + `anie-agent`.

Estimated diff: ~150 LOC of code, ~80 LOC of tests.

## Phased PRs

Single PR.

## Test plan

- `recurse_tool_logs_depth_on_each_invocation` — assert
  the info-level log line shape via `tracing_test`.
- `recurse_depth_propagates_through_sub_agent_factory`
  — construct factory, build a sub-agent at depth 2,
  verify the sub-agent's own factory sees depth 3.
- `recurse_depth_warn_threshold_emits_system_message`
  — install a 3-deep call chain, verify a
  `SystemMessage` is emitted at the configured
  threshold.
- `recurse_depth_does_not_abort_when_threshold_exceeded`
  — even at depth 100, the run continues.
- `recurse_depth_status_bar_segment_updates_with_max_seen`
  — a TUI snapshot or status-render test confirming
  the segment renders.

## Risks

- **Cargo-cult depth tracking.** Adding a field
  without a reason to read it. Mitigation: PR 4
  (decompose) is the first real consumer; PR 2 (tool
  inheritance) is when the field becomes interesting.
  PR 1 alone is observability-prep.
- **Status-bar clutter.** Adding another segment.
  Mitigation: only render when depth > 0 (silent in
  the common case).

## Exit criteria

- [ ] `SubAgentBuildContext` carries `depth: u32`.
- [ ] `RecurseTool` logs depth on every fire.
- [ ] `ANIE_RECURSE_DEPTH_WARN_AT` env override
      respected; default 5.
- [ ] Warning fires once per
      `(scope.kind, depth)` pair (throttle, mirroring
      the failure-loop detector pattern).
- [ ] All five tests above pass.
- [ ] `cargo test --workspace` + `cargo clippy
      --workspace --all-targets -- -D warnings` clean.
- [ ] Smoke run shows `depth=0` for all top-level
      recurse calls (sanity).

## Deferred

- Depth-aware tool selection (only re-include `recurse`
  in sub-agent registry when `depth < N`). Lands in
  PR 2 alongside the rest of tool inheritance.
- Hard depth cap (`abort_at` knob). Add only if smoke
  shows runaway recursion that observability alone
  can't surface meaningfully.
