# Plan 06 — Quick wins (autocomplete, idle, mouse)

## Rationale

Three small issues the audit caught that are trivially fixable
and plausibly contribute to perceived sluggishness independent
of the bigger caching work. Each is small enough to not need
its own plan folder, but they're grouped here so the PRs land
together and the before/after smoke test is one thing.

None of these individually will move a flamegraph needle. In
aggregate they fix specific "feels laggy" moments that block
caching doesn't address.

## Issues and fixes

### 6.1 Autocomplete: debounce provider queries

**Audit finding:** `input.rs:145` — `refresh_autocomplete()`
runs on every keystroke. If the suggestion provider does
nontrivial work (filtering a large model catalog, running a
fuzzy match across N entries), keystroke latency tracks the
provider's cost.

**Fix:** debounce at ~80 ms. Store a `pending_refresh:
Option<Instant>` on `InputPane`; on keystroke, set to
`Instant::now() + DEBOUNCE`. The render tick checks if the
pending time has elapsed and, if so, runs the refresh and
clears the pending marker. Rapid typing produces one refresh
instead of one per keystroke.

For very fast suggestions (cheap in-memory fuzzy on a
pre-built index) this is wasted work, but 80 ms is below
perception for typing and the refresh still fires before the
user finishes reading what they typed.

### 6.2 Spinner: suppress idle redraws when no token arrived

**Audit finding:** `app.rs:213–220, 623–628`. The spinner
advances every 80 ms. `needs_tick_redraw()` returns `true`
whenever `AgentUiState` is `Streaming` or `ToolExecuting`,
forcing a `build_lines` call even if no new token arrived.
At 80 ms cadence, that's 12 full rebuilds per second for a
stalled stream.

**Fix:** track `last_streaming_delta_at: Instant`. If no
delta has arrived in the last 500 ms, skip the spinner redraw
(the spinner will briefly appear frozen, which is
accurate — the stream *is* stalled). Resume spinner
animation on the next delta.

This costs a ~500 ms delay before the spinner visually
reflects a stall, which is desirable: a frozen spinner
communicates "nothing is happening" better than an animated
spinner does.

### 6.3 Mouse motion: verify the filter actually fires

**Audit finding:** `app.rs:1497–1514`. Mouse-motion events are
filtered from `affects_render` but *still drained* at up to
100 events/sec. The Agent-D audit suggests the drain loop
may re-enter 100×/sec even when no redraw happens.

**Fix:** verify the filter. Add a `tracing::trace!` on every
drained mouse-motion event gated behind `ANIE_PERF_TRACE=1`.
If the trace confirms events are being drained-and-ignored at
high rate, consider filtering at the `crossterm::event::read`
level via `event_filter` (crossterm supports this) so the
events never enter the loop.

Three outcomes:
- Filter is correct and events never arrive: no change
  needed; document and close.
- Events arrive but affect nothing observable: keep the
  drain loop cheap (one enum match, no work).
- Events cause real delay: add `event_filter` at crossterm
  level.

This is **investigatory** — we don't commit to a fix until
the trace confirms what's actually happening.

## Files to touch

- `crates/anie-tui/src/input.rs`: autocomplete debounce.
- `crates/anie-tui/src/app.rs`:
  - `last_streaming_delta_at` tracking + `needs_tick_redraw`
    update.
  - Mouse-motion trace.
- Tests per the plan below.

## Phased PRs

### PR-A: autocomplete debounce

Small, isolated. Lands first.

- Test: 10 keystrokes within 50 ms produce one
  `refresh_autocomplete` call, not 10.
- Test: 1 keystroke followed by 100 ms of idle produces one
  call.

### PR-B: stall-aware spinner

- Test: no deltas for 600 ms with state=Streaming — spinner
  frame does not advance, no `build_lines` called.
- Test: delta arrives — spinner resumes on next tick.

### PR-C: mouse-motion investigation + fix

- First commit: add the gated trace.
- Collect data from a 30-second session with active mouse
  movement.
- Second commit (only if trace shows real volume): switch to
  `event_filter` or document that the existing drain is
  correct and close.

## Test plan

Covered per-PR above. Aggregate check: the three scenarios in
the Plan 01 bench (scroll, stream, resize) show no regression.

## Risks

- **Autocomplete debounce feels sluggish.** 80 ms is below
  typing perception for most users. If a user complains, we
  shrink to 40 ms or remove. Fundamentally optional.
- **Stall suppression hides a real bug.** If the provider
  genuinely hangs, users should still see *something*
  happening. Mitigation: after `STALL_WARN_MS` (3 sec) of
  no deltas, emit a "waiting for model response…" status
  line — orthogonal feature, opens a follow-up.
- **Mouse filter removal breaks scroll-wheel.** Wheel events
  are *not* motion events in crossterm (`MouseEventKind::
  ScrollUp`/`ScrollDown`). Double-check the filter is motion-
  only, not all mouse.

## Exit criteria

- [ ] PR-A landed. Debounce test passes.
- [ ] PR-B landed. Stalled-stream spinner test passes.
- [ ] PR-C investigation landed. Either: (a) a fix PR with
      evidence; or (b) a close-out comment citing the trace
      data showing no action needed.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] Subjective smoke: typing fast in the composer feels
      instant; a stalled stream doesn't waste frames.

## Deferred

- **"Waiting for model response…" warning** after long
  stalls. Orthogonal feature.
- **Prefetch suggestion provider results** on common
  prefixes (e.g., precompute `/` results). Not needed unless
  debounce proves inadequate.
- **Global input-response SLO** (input-to-paint latency
  target). The Plan 01 bench covers this; no separate budget
  is needed in this plan.
