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
| 01 | Context-aware compaction reserve | Done (PR A) | `db94e13` (effective_reserve helper + applied in compaction_strategy). PRs B/C (min_reserve_tokens config + /state rendering) deferred. |
| 02 | Per-turn compaction budget | Done (PRs A + B) | `1b16ffe` (counter + max_per_turn config + GiveUpReason::CompactionBudgetExhausted). PR C (mid-turn budget consultation) folded into 04 PR B. |
| 03 | Agent-loop compaction signal | Done | `65248c1` (CompactionGate trait + AgentLoopConfig::with_compaction_gate; default off). |
| 04 | Mid-turn compaction execution | Done (PRs A + B) | `a000ba4` (compact_messages_inline + estimate_message_tokens). `31adf29` (ControllerCompactionGate + build_agent integration). PR C (manual smoke + docs) deferred. |
| 05 | Tool output caps scale with context | Done (PRs A–D) | `8f03142` (PR A: ToolExecutionContext through Tool::execute), `5a79ec6` (PR B: bash effective_tool_output_budget), `5fcbb53` (PR C: read), `4237baa` (PR D: web_read). The `[tools] context_share_for_output` config knob is deferred — share is hardcoded at 10 % for now; revisit if real workloads need a different ratio. |
| 06 | Compaction telemetry and visibility | Done (PRs A–C) | `33f5116` (PR A: CompactionPhase enum + event plumbing), `16a8e1b` (PR B: CompactionStats counters + `/state` rendering), `31158e6` (PR C: TUI activity-row phase labels). PR D (`/compaction-stats` slash command) deferred — `/state` already surfaces the same data. Skipped-budget-exhausted SystemMessage breadcrumbs (plan §"Surfacing skipped compactions") covered by the existing reactive-budget message and the gate's `Skipped { reason }` pathway. |

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

- [x] Effective reserve scales for small windows. (Plan 01 PR A)
- [x] Per-turn compaction budget enforced across all three trigger paths. (Plan 02 PRs A + B; mid-turn budget consult folded into 04 PR B)
- [x] Agent-loop hook installed; mid-turn compaction fires when warranted. (Plan 03; Plan 04 PRs A + B)
- [x] Tool outputs shrink for small context windows. (Plan 05 PRs A–D, covers bash, read, web_read; other tools inherit via `ToolExecutionContext` and can opt in incrementally)
- [x] Telemetry distinguishes pre-prompt / mid-turn / reactive. (Plan 06 PRs A–C)
- [ ] Manual smoke of small-context Ollama coding task completes
      with at least one mid-turn compaction observed. (Deferred to next live-session smoke pass — code paths exercised by unit + integration tests across the workspace.)
- [x] No regression for cloud-model golden tests. (Validated via `cargo test --workspace` + the `bash_tool_keeps_larger_budget_for_cloud_window` and `read_tool_keeps_full_output_for_cloud_window` regression guards.)
