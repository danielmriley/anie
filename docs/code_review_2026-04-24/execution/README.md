# code_review_2026-04-24 execution tracker

This file tracks implementation status for
`docs/code_review_2026-04-24/`. The plan files are the source of design;
this tracker is the source of landing status.

## Status legend

- **Pending** — not started.
- **In progress** — implementation branch/PR underway.
- **Done** — landed and validated.
- **Deferred** — intentionally not doing now; rationale required.

## Plan status

| # | Plan | Status | Notes |
|---|------|--------|-------|
| 01 | OAuth refresh lock async isolation | Pending | First recommended implementation target. |
| 02 | OpenAI structured image serialization | Pending | Concrete provider compatibility gap. |
| 03 | Retry-state consistency | Pending | Needs policy choice: cancel armed retry or reject change. Recommended: cancel retry with system marker. |
| 04 | OAuth callback deadlines | Pending | Lower reference confidence than 01, but the accepted-stream timeout gap is real. |
| 05 | Anthropic truncation classification | Pending | Prefer provider-local raw stop-reason tracking unless broader protocol need appears. |
| 06 | Runtime-state persistence visibility | Pending | Land before deeper atomic-write changes so failures have a visible route. |
| 07 | Atomic write hardening | Pending | Includes same-process temp uniqueness and Windows replacement semantics. |
| 08 | Tool edit resource caps | Pending | Runtime checks required; schema is only guidance. |
| 09 | TUI agent-event drain bounds | Pending | Coordinate with TUI performance docs and benchmarks. |
| 10 | Repo hygiene, CI, and print-mode error visibility | Pending | Safe cleanup track after high-priority runtime/provider work. |

## Completed pre-work

| Item | Status |
|---|---|
| Canonical architecture doc | Done |
| README architecture summary | Done |
| Full-access tool behavior documented | Done |
| Windows `ls.rs` import fix | Done |
| Bash deny policy | Done |
| pi/Codex comparison synthesis | Done |

## Validation gates

Every implementation PR should run the targeted tests listed in its
plan. Before marking a plan done, run:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Additional gates by plan:

| Plan | Extra validation |
|---|---|
| 01 | Contended OAuth refresh smoke or regression test. |
| 02 | Request-body regression test showing OpenAI `image_url` content parts. |
| 03 | Controller tests for model and thinking changes during armed retry. |
| 04 | Idle callback connection regression test. |
| 05 | Anthropic SSE/stream-state fixture for `max_tokens` with no visible content. |
| 06 | Persistence failure injection test that surfaces a warning. |
| 07 | Concurrent same-process write test; Windows target check. |
| 08 | Oversized edit argument/file/output tests. |
| 09 | TUI burst test or benchmark smoke. |
| 10 | CI workflow validation; print-mode stderr test for final errors. |

## Deferral rules

If a plan is deferred, update both this tracker and the plan file with:

- why it is deferred
- what evidence would restart it
- whether any architecture doc text needs to change

