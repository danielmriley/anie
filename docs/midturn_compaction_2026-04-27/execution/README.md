# midturn_compaction_2026-04-27 execution tracker

This file tracks implementation status for the plans in
`docs/midturn_compaction_2026-04-27/`. Plan files own design;
this tracker owns landing status.

## Status legend

- **Pending** — not started.
- **In progress** — implementation underway.
- **Done** — landed and validated.
- **Deferred** — intentionally not doing now; rationale required.

## Plan status

| # | Plan | Status | Notes |
|---|------|--------|-------|
| 00 | Baseline analysis | Reference doc | Cite-anchor for the plan set; not a PR. |
| 01 | Context-aware compaction reserve | Pending | Foundation for 04 and 05. |
| 02 | Per-turn compaction budget | Pending | Anti-thrash; needed before 04. |
| 03 | Agent-loop compaction signal | Pending | Pure plumbing; unblocks 04. |
| 04 | Mid-turn compaction execution | Pending | Load-bearing change. Depends on 01, 02, 03. |
| 05 | Tool output caps scale with context | Pending | Independent of 04 in implementation; complementary in effect. |
| 06 | Compaction telemetry and visibility | Pending | Lands last, against stable mid-turn machinery. |

## PR ordering

Suggested:

1. 01 PR A (effective reserve helper + apply at call site).
2. 02 PR A (budget counter, no enforcement yet).
3. 02 PR B (reactive path enforces the budget).
4. 03 PR (agent-loop hook, default off).
5. 04 PR A (refactor `compact_internal` into pure helper).
6. 04 PR B (install `ControllerCompactionGate`).
7. 02 PR C (mid-turn path consults the budget).
8. 04 PR C (manual smoke + docs).
9. 05 PR A (plumb `context_window` into tool execution context).
10. 05 PRs B/C/D (apply effective budget to bash, read, web_read).
11. 06 PR A (`CompactionPhase` enum).
12. 06 PRs B/C/D (counters, TUI labels, optional slash command).
13. 01 PRs B/C (optional `min_reserve_tokens`, `/state` rendering).

01 PR A and 02 PR A can land in either order; both are tiny. The
critical path is 03 → 04 PR A → 04 PR B for the actual mid-turn
behavior. Telemetry (06) may be reordered earlier if observability
during 04's rollout would help diagnose issues.

## Validation gates

Per PR (unless docs-only):

- `cargo fmt --all -- --check`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`

Targeted gates by plan:

| Plan | Extra validation |
|---|---|
| 01 | Property test on `effective_reserve` over a range of windows. |
| 02 | Compaction-storm fault-injection test. |
| 03 | Agent run with no gate is byte-identical to a baseline replay. |
| 04 | Small-context Ollama smoke run with mid-turn compaction observed. |
| 05 | Cloud-vs-local regression (200K window keeps modest tightening; 8K window aggressively shrinks tool output). |
| 06 | Forward-compat session-log load test. |

## Milestone exit

- [ ] Effective reserve scales for small windows.
- [ ] Per-turn compaction budget enforced across all three trigger paths.
- [ ] Agent-loop hook installed; mid-turn compaction fires when warranted.
- [ ] Tool outputs shrink for small context windows.
- [ ] Telemetry distinguishes pre-prompt / mid-turn / reactive.
- [ ] Manual smoke of small-context Ollama coding task completes
      with at least one mid-turn compaction observed.
- [ ] No regression for cloud-model golden tests.
