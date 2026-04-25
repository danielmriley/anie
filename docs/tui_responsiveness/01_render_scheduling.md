# PR 1 — Render scheduling with 30 FPS cap

**Goal:** Stop drawing every time the main loop iterates. Draw at
most 30 times per second, only when something visible has
changed. Bound the worst-case redraw rate regardless of how fast
the agent emits events.

## Current behavior

After the partial fix in `df2d2c4`, the loop drains all pending
agent events before redrawing and suppresses idle ticks. But
there is no rate cap. A rapid stream of input events (user
holding a key, terminal resize storm, flurry of controller
events that each individually arrive at different times) can
still produce more redraws per second than is useful, and each
redraw currently re-wraps the entire transcript.

## Design

Adopt pi's request-based model (`pi/tui.ts::requestRender` +
`scheduleRender`) but translated to tokio:

```rust
const FRAME_BUDGET: Duration = Duration::from_millis(33); // ~30 fps
const IDLE_TICK: Duration = Duration::from_millis(100);

pub async fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut term_events = EventStream::new();
    let mut dirty = true;
    let mut last_render_at = Instant::now()
        .checked_sub(FRAME_BUDGET)
        .unwrap_or_else(Instant::now); // force first frame

    loop {
        // Render if dirty AND we've respected the frame budget.
        // This caps the redraw rate at ~30 fps regardless of
        // event rate — the rate cap is the substantive change.
        if dirty && last_render_at.elapsed() >= FRAME_BUDGET {
            terminal.draw(|frame| app.render(frame))?;
            dirty = false;
            last_render_at = Instant::now();
        }

        // Compute the deadline for the next render opportunity.
        // If we're dirty but under budget, that's the time until
        // the budget window opens. If we're clean, that's the
        // idle-tick interval.
        let timeout = if dirty {
            FRAME_BUDGET.saturating_sub(last_render_at.elapsed())
        } else {
            IDLE_TICK
        };

        tokio::select! {
            Some(Ok(event)) = term_events.next() => {
                app.handle_terminal_event(event)?;
                dirty = true;
            }
            Some(event) = app.event_rx.recv() => {
                app.handle_agent_event(event)?;
                while let Ok(event) = app.event_rx.try_recv() {
                    app.handle_agent_event(event)?;
                }
                dirty = true;
            }
            _ = tokio::time::sleep(timeout) => {
                // Either the frame budget elapsed (we'll draw
                // next iteration) or the idle tick fired.
                app.handle_tick()?;
                if app.needs_tick_redraw() {
                    dirty = true;
                }
            }
        }

        if app.should_quit() {
            break;
        }
    }
    Ok(())
}
```

Key moves:

- **Rate cap via `last_render_at` + `FRAME_BUDGET`.** If a state
  change happens 5 ms after the last render, we mark `dirty` but
  wait 28 ms before drawing. Input events keep arriving and get
  handled; they just all coalesce into one paint. At 30 fps this
  is imperceptible to the user but can collapse dozens of
  per-event redraws into one.
- **Dynamic select timeout.** When `dirty && under budget`, the
  sleep branch fires exactly when the next paint should happen.
  When clean, it falls back to the 100 ms idle tick. No busy
  wait.
- **First frame is forced.** Subtracting `FRAME_BUDGET` from the
  initial timestamp means the first `dirty && elapsed >= budget`
  check passes immediately — the UI appears with no 33 ms delay
  on startup.
- **Tick handling unchanged.** `handle_tick()` still polls
  overlay workers; `needs_tick_redraw()` still gates whether the
  spinner animation needs a frame.

## Why not `tokio::time::interval`?

A 30 Hz interval would tick 30 times per second regardless of
whether anything changed. We want "at most 30 fps, zero fps when
idle." A manual `last_render_at` check is simpler and sleeps the
runtime when nothing is happening.

## Why 30 fps, not 60?

The user requested 30 explicitly. A terminal UI with no
animation faster than a spinner doesn't benefit from 60 fps; 30
is a reasonable ceiling that halves the worst-case redraw cost
and leaves headroom for the per-block cache in PR 2. Pi uses
~60 (`MIN_RENDER_INTERVAL_MS = 16`) but pi also has a per-
component cache in place, so its per-frame cost is much lower
than ours.

## Files

- `crates/anie-tui/src/app.rs` — `run_tui` only.

No changes needed to `App::render` itself, `handle_*` methods,
or any overlay code.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | `frame_budget_rate_caps_redraws_under_burst` | Drive App with a synthetic stream of 100 MessageDelta events landing within 10 ms; assert the terminal backend received <= ceil(10 ms / 33 ms) + 1 full draws. Requires access to a `TestBackend` or a render counter. |
| 2 | `idle_app_does_not_redraw` | Start App in Idle state with no overlay; advance tokio time by 1 second; assert 0 redraws occurred after the first. |
| 3 | `dirty_flag_survives_tick_that_needs_no_redraw` | Mark dirty, advance time by less than `FRAME_BUDGET`, fire an idle tick; assert we're still dirty after the tick so the next budget window draws once. |
| 4 | `first_frame_draws_immediately` | Start App, advance tokio by < `FRAME_BUDGET`; assert a draw happened (no 33 ms startup stall). |

Tests 1-4 need a render counter — the simplest path is to thread
a shared `Arc<AtomicU64>` into the test App that increments
inside the draw closure. PR 3 will formalize this counter; PR 1
can use a test-only variant.

If test infrastructure for real integration testing turns out to
be awkward, it's acceptable to ship PR 1 with behavioral tests
that assert `dirty` state transitions in isolation and manually
verify rate capping with `ANIE_DEBUG_REDRAW=1` once PR 3 lands.
Document that choice in the PR description.

## Exit criteria

- [ ] `run_tui` rate-caps redraws at 30 fps.
- [ ] First frame is not delayed by the frame budget.
- [ ] Existing TUI tests pass unchanged (the loop behavior is
      observable only through rendered frames, not through
      `handle_*` methods, so unit tests that poke App directly
      stay valid).
- [ ] Tests 1-4 pass (or the choice to defer to PR 3's
      instrumentation is documented).
- [ ] Manual: long agent run followed by a keystroke-heavy
      session shows no input lag.
