# Plan 08 — compaction visibility

> **Status (2026-04-25):** PR A landed in `9a9e1e9`
> (`AgentUiState::Compacting { started_at }`, spinner +
> elapsed counter rendered by `render_spinner_row`). PR B
> (split-turn label) is **deferred** — `CompactionStart`
> still has no payload and no user has asked for the "(2
> summaries)" disclosure. Reopen if split-turn compaction
> becomes a frequent user-visible delay.

**Polish. Surfaces background work that was silent before.**

After PR 07G's ordering fix, the "Compacting context…" system
message fires before the LLM summarization call. Good — the user
knows a compaction started. Bad — the message is static, and the
LLM call can take 30+ seconds on a long transcript. The user
stares at a frozen line, wondering if something hung.

## Rationale

Two observed gaps:

1. **No elapsed counter.** Users can't tell if compaction has
   been running for 2 s or 20 s.
2. **No progress hint on split-turn compaction.** Plan 06's
   split-turn path runs two parallel summarization calls
   (`tokio::try_join!`), meaning total time can be longer still.
   The user doesn't know this happens.

Both are solvable with existing machinery — `Spinner` is already
used for streaming answers, and we already emit `CompactionStart`
/ `CompactionEnd` events. The missing piece is a running "tick"
while compaction is in-flight.

## Design

### Approach

Add an `AgentUiState::Compacting { started_at: Instant }`
variant alongside `Idle`, `Streaming`, `ToolExecuting`. When
`CompactionStart` fires, transition to `Compacting`. The status
bar renders `"⠋ Compacting context… 7s"` using the existing
`Spinner::frame()` pulse and `Instant::elapsed()`. When
`CompactionEnd` fires, transition back to `Idle` and the static
system message stays in the transcript as the permanent record.

### Why not a new AgentEvent

We'd need `CompactionTick`s at 10 Hz pushed from the controller,
which re-spins the frame-rate throttle discussion. The
alternative — the TUI ticks its own spinner on each redraw — is
cheaper and matches how the streaming spinner works today
(`Spinner::frame()` is called in `OutputPane::render`).

### Split-turn signaling

`CompactionStart` today carries no payload. Extend it to
`CompactionStart { split_turn: bool }` so the status-bar copy
can change to "Compacting split turn (2 summaries)…" when pi's
two-LLM-call path is in play. Purely cosmetic but informative.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-protocol/src/event.rs` (or wherever `AgentEvent` lives) | `CompactionStart { split_turn: bool }`. |
| `crates/anie-cli/src/controller.rs` | Pass `cut_point.split_turn.is_some()` on the emit. |
| `crates/anie-tui/src/app.rs` | `AgentUiState::Compacting { started_at, split_turn }`. |
| `crates/anie-tui/src/status_bar.rs` (or wherever the bar renders) | Show spinner + elapsed + split-turn note. |

## Phased PRs

### PR A — elapsed counter + spinner

1. `AgentUiState::Compacting { started_at: Instant }`.
2. Controller emits `CompactionStart` (unchanged shape for now).
3. TUI transitions to Compacting on start, back to Idle on end.
4. Status bar renders spinner + elapsed.
5. Tests: event transitions + status-bar rendering snapshot.

### PR B — split-turn label (deferred)

1. Thread `split_turn: bool` through `CompactionStart`.
2. Status bar appends "(2 summaries)" when true.
3. Test: split-turn emit sets the flag.

Deferred 2026-04-25: optional polish, no user request, and
the elapsed counter from PR A already covers "is this still
running?" which was the original gap.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | `compaction_start_transitions_app_to_compacting_state` | `anie-tui` tests |
| 2 | `compaction_end_transitions_back_to_idle` | same |
| 3 | `maybe_auto_compact_emits_split_turn_flag_when_applicable` | `anie-cli::controller` |

## Risks

- **Spinner frame churn.** The status-bar spinner ticks each
  render. The existing streaming path already does this with the
  30-fps cap from `tui_responsiveness`; compaction reuses it
  unchanged.
- **Emit ordering in force_compact.** `force_compact` already
  emits `CompactionStart` before the LLM call. No new ordering
  work needed for `/compact` manual invocation.

## Exit criteria

- [ ] PR A merged.
- [ ] Manual: run a session past the threshold; status bar
      shows "Compacting context… Ns" with N increasing each
      second.
- [ ] After CompactionEnd, the system message in the transcript
      stays (permanent record) and the status bar returns to
      normal.

## Deferred

- **Per-call progress during split-turn.** Knowing main-summary
  vs. prefix-summary currently in flight would require
  `tokio::join!` → individual completion events. Not worth the
  plumbing until someone asks.
