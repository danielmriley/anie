# 06 — Compaction telemetry and visibility

## Rationale

Once mid-turn compaction is live (plan 04), three different code
paths trigger compaction:

1. Pre-prompt (existing, `maybe_auto_compact`).
2. Mid-turn (new, `ControllerCompactionGate`).
3. Reactive on `ContextOverflow` (existing,
   `RetryDecision::Compact`).

Diagnosing context-pressure problems in a real session means
knowing how often each fires, how much context they free, and
which one is doing the heavy lifting. Without telemetry, "why is
my session slow today" is unanswerable.

## Design

### Per-session counters

Add to `InteractiveController`:

```rust
struct CompactionStats {
    pre_prompt: u32,
    mid_turn: u32,
    reactive_overflow: u32,
    skipped_budget_exhausted: u32,
    skipped_no_op: u32,  // compaction ran but freed < 10 % of context
}
```

Updated by the existing `emit_compaction_end` site
(`crates/anie-cli/src/controller.rs:908-923`) plus the new
mid-turn path. The variant determines which counter to bump,
inferred from the `CompactionPhase` we'll attach to events
(below).

### `CompactionPhase` on the event

Codex's `CompactionPhase` enum is the cleanest model. Add to
anie's protocol:

```rust
pub enum CompactionPhase {
    PrePrompt,
    MidTurn,
    ReactiveOverflow,
}
```

Extend `AgentEvent::CompactionStart` and
`AgentEvent::CompactionEnd` with `phase: CompactionPhase`. The
TUI uses it to surface a more specific status:

- `PrePrompt` → "compacting Xs" (current behavior).
- `MidTurn` → "compacting (mid-turn) Xs".
- `ReactiveOverflow` → "compacting after overflow Xs".

Three different glyphs/labels turn the activity row into a
diagnostic without requiring the user to inspect logs.

### Structured tracing

Each compaction emits a single tracing event at INFO level:

```rust
tracing::info!(
    phase = ?phase,
    tokens_before,
    tokens_after,
    bytes_freed = tokens_before.saturating_sub(tokens_after),
    duration_ms,
    budget_remaining,
    "compaction"
);
```

Tagged `target = "anie_cli::compaction"` so it can be filtered
out of noisy logs. The existing `anie.log` file picks these up
automatically via the workspace tracing-subscriber config.

### `/state` and `/compaction-stats` slash commands

Extend the `/state` summary to include compaction stats:

```text
Session: c974f743 (qwen3.6:latest @ 65,536 ctx, reserve 16,384 effective)
Compactions this session: 4 (pre-prompt: 2, mid-turn: 2, overflow: 0)
                          1 skipped (budget exhausted)
```

Optionally add a dedicated `/compaction-stats` command that prints
just this block plus the current per-turn budget state.

### Surfacing skipped compactions

When the budget is exhausted (plan 02) or compaction would be a
no-op, emit a `SystemMessage` to the transcript so the user
sees the decision in context. Examples:

- `"Skipped mid-turn compaction: budget exhausted (2/2)."`
- `"Skipped mid-turn compaction: previous compaction freed only
  3 % of context."`

The system messages are dim and non-disruptive; they're a
breadcrumb trail, not a notification.

## Files to touch

- `crates/anie-protocol/src/lib.rs` (or wherever `AgentEvent`
  lives)
  - Add `CompactionPhase` enum.
  - Extend `CompactionStart` and `CompactionEnd` with
    `phase: CompactionPhase`.
- `crates/anie-cli/src/controller.rs`
  - Add `CompactionStats` field to `InteractiveController`.
  - Increment from `emit_compaction_end` based on phase.
  - Wire `CompactionPhase` argument into every callsite that
    emits a compaction event (pre-prompt, mid-turn, reactive).
- `crates/anie-tui/src/app.rs`
  - Update activity-row label rendering to read the phase from
    the most recent `CompactionStart` event.
- `crates/anie-cli/src/state_summary.rs` (or wherever the `/state`
  text lives)
  - Render the stats block.
- `crates/anie-cli/src/commands.rs` (slash command registry)
  - Add `/compaction-stats` if we want the dedicated command.

## Phased PRs

### PR A — `CompactionPhase` enum + plumbing

**Change:**

- Add the enum.
- Extend the two events.
- Plumb `phase` into the existing pre-prompt callsite.
- Default mid-turn / reactive to placeholder values until those
  paths consume them in their own PRs.

**Tests:**

- `compaction_event_serializes_phase_field`
- `compaction_phase_round_trip_through_session_log`
  (forward-compat: new field defaults appropriately when an
  older-schema entry is loaded).

**Exit criteria:**

- Pre-prompt compactions still work; events now carry `phase`.

### PR B — `CompactionStats` counters

**Change:**

- Add the struct.
- Update from `emit_compaction_end`.
- Reset on `Session::clear` (manual /reset).
- Surface in `/state`.

**Tests:**

- `compaction_stats_increments_pre_prompt_counter`
- `compaction_stats_resets_with_session_clear`
- `state_summary_includes_compaction_stats`

**Exit criteria:**

- `/state` shows compaction counts; counters survive multiple
  turns.

### PR C — TUI activity-row labels per phase

**Change:**

- Track the most recent `CompactionPhase` on `App` state.
- `render_spinner_row` picks a label based on the phase.

**Tests:**

- `activity_row_shows_pre_prompt_label_for_pre_prompt_phase`
- `activity_row_shows_midturn_label_for_midturn_phase`

**Exit criteria:**

- Mid-turn compactions are visually distinguishable in the TUI.

### PR D — Optional `/compaction-stats` command

**Change:**

- Slash command that prints the stats block.

**Tests:**

- `compaction_stats_command_emits_expected_format`

**Exit criteria:**

- Diagnostic available without typing `/state`.

## Test plan

End-to-end (in `anie-integration-tests`): drive a fake provider
through three turns with mixed compaction triggers (pre-prompt,
mid-turn, reactive overflow) and assert
`CompactionStats { pre_prompt: 1, mid_turn: 1,
reactive_overflow: 1, .. }` after the run.

## Risks

- **Schema bump on session log.** Adding `phase` to compaction
  events touches the persisted session schema. Forward-compat
  test required: an older-schema entry without `phase` must
  load with a sensible default (e.g. `PrePrompt`).
- **TUI churn.** Three different labels in the activity row is a
  small UX surface. Keep the styling consistent (yellow + dim,
  matching the existing "Responding" treatment) so it doesn't
  feel disjoint.

## Exit criteria

- [ ] Every compaction emits an event tagged with its
      `CompactionPhase`.
- [ ] `CompactionStats` counters track pre-prompt, mid-turn,
      reactive, and skipped compactions per session.
- [ ] `/state` shows the counts.
- [ ] TUI activity row distinguishes phases visually.
- [ ] Forward-compat: pre-PR session entries load cleanly.
- [ ] `cargo test --workspace`, clippy clean.

## Deferred

- **Persistent metrics across sessions.** "Anie has compacted
  327 times this week." Speculative; nice-to-have.
- **Compaction effectiveness ratio.** "Average freed: 42 %".
  Useful diagnostic but cosmetic; can be derived from the
  structured tracing logs offline.
- **Surfacing the phase in the session-log JSON for replay.**
  The forward-compat test handles backward compatibility; we
  intentionally don't backfill `phase` on existing entries.
