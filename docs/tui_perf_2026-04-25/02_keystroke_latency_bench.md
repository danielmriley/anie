# 02 — Add a keystroke-to-paint latency benchmark

## Rationale

The existing `tui_render` criterion bench
(`crates/anie-tui/benches/tui_render.rs`) constructs an
`OutputPane` directly, drives it against a `TestBackend`, and
times `OutputPane::render`. The pipeline elements the user
actually feels — `App::render_with_mode`, `InputPane::render`
including the doubled `layout_lines`, `App::render_status_bar`
including `shorten_path`, the keystroke-handling tokio select
loop, the agent-event drain interleaving — are all bypassed.

This is why the criterion bench numbers are stable but the user
still feels lag. The bench measures a slice of the system that's
already optimized; the slice that hurts is unmeasured.

This PR closes that gap. Without it, every fix in PR 01, 03, 04
is graded on subjective feel, which doesn't ratchet against
regressions.

## Design

### What to measure

**End-to-end keystroke→paint latency** in the full `App`-driven
pipeline:
- A pre-built `App` with a 600-block transcript and (for some
  scenarios) an active streaming block.
- Inject `Event::Key(KeyCode::Char(c), KeyModifiers::NONE)` via
  the existing `App::handle_terminal_event`.
- Time the path from input arrival to `terminal.draw_urgent`
  return.
- Sample at criterion's normal cadence; compute mean + p95.

The existing `ANIE_TRACE_TYPING` instrumentation
(`crates/anie-tui/src/app.rs:1901-1908`) already records this
exact span. Reuse the timing logic in the bench harness rather
than re-deriving it.

### Scenarios

1. **`keystroke_into_idle_app_600`** — 600 finalized blocks, no
   streaming, pre-warmed cache. Type one character. This is the
   common "I'm typing my next prompt" case.
2. **`keystroke_during_stream_600`** — 600 finalized blocks +
   one active streaming block (continuously appending 5-char
   deltas). Type one character. This is the "agent is answering,
   I'm queuing another input" case where the user feels lag
   most.
3. **`keystroke_into_long_buffer`** — same as #1 but with a
   pre-filled 200-character input buffer. Targets Finding F-1
   (`layout_lines` cost scales with buffer length).

### Why three scenarios

- #1 isolates per-frame cost on the urgent paint path — Idle
  state, no active streaming work to interleave.
- #2 verifies the urgent paint actually does decouple from
  ongoing transcript work.
- #3 is the dedicated regression guard for the input-pane fix
  (PR 01). Without it, that PR is ungated.

### Headless harness

Use ratatui's `TestBackend` like the existing bench. Drive the
`App` event handler directly:

```rust
fn time_keystroke(app: &mut App, terminal: &mut Terminal<TestBackend>) -> Duration {
    let start = Instant::now();
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('a'), KeyModifiers::NONE,
    ))).expect("handle_terminal_event");
    terminal.draw(|f| app.render_urgent_for_test(f)).expect("draw");
    start.elapsed()
}
```

`render_urgent_for_test` (`anie-tui/src/app.rs:591-593`) already
exists for this kind of test injection.

The bench iterates this; criterion handles statistics. We don't
need to wire DECSET 2026 / `draw_urgent` — that's terminal-side
machinery that doesn't run against `TestBackend`.

## Files to touch

- `crates/anie-tui/benches/tui_render.rs` — add three new bench
  functions (or split into a sibling `tui_input.rs` if the file
  grows past ~250 lines; not yet).
- `crates/anie-tui/src/app.rs` — possibly extend
  `render_urgent_for_test` to be reachable from the bench (it
  is currently `#[cfg(test)]` only). Either expose it as
  `pub(crate)` behind a `cfg(any(test, feature = "bench-helpers"))`
  flag, or move the bench helper into a public test-fixture
  module. The cleanest path is a small `pub fn for_bench(...)`
  on `App` that the bench file is allowed to call — criterion
  benches compile against the public API by default.
- `crates/anie-tui/Cargo.toml` — register the new benches.
- `docs/tui_perf_2026-04-25/00_report.md` — append the new
  baseline numbers once captured.

## Phased PRs

Single PR. The harness change is small and doesn't depend on
any of the fix PRs.

## Test plan

1. **`bench_compiles_and_runs`** — the bench must build and run
   on `cargo bench -p anie-tui --bench tui_render`. Criterion
   compares against itself per run; no assertion needed in code.
2. **Baseline capture** — record numbers in
   `00_report.md` and `tui_perf_architecture/execution/baseline_numbers.md`
   so future PRs have a fixed reference.
3. **PR 01 follow-up gate** — once PR 01 lands, the
   `keystroke_into_long_buffer` scenario must improve by ≥30%
   on a 200-char buffer. (Doubled O(n) → singled O(n) is a 50%
   theoretical improvement on the layout step; 30% allows for
   surrounding render cost.)

## Risks

- **`TestBackend` doesn't model GPU sync wrap.** The
  `draw_urgent` vs. `draw_synchronized` distinction
  (`b125f98`) is invisible to `TestBackend`. The bench measures
  Rust-side cost only, not terminal-side latency. Worth calling
  out in the bench file's doc-comment so future readers don't
  expect it to catch terminal-side regressions.
- **Bench setup cost.** Building a 600-block transcript inside
  `App` is more setup than the existing pure-OutputPane bench.
  Use criterion's `iter_with_setup` or a per-scenario
  warm-build to keep the timed region tight.
- **The `App` constructor takes channels.** `App::new` requires
  an `event_rx` (`mpsc::Receiver<AgentEvent>`) and an
  `action_tx` (`mpsc::UnboundedSender<UiAction>`). The bench
  needs dummy halves; create them once per scenario and ignore
  the action stream.

## Exit criteria

- `cargo bench -p anie-tui --bench tui_render` runs all six
  scenarios (3 existing + 3 new).
- Baseline numbers written into
  `tui_perf_2026-04-25/00_report.md` and
  `tui_perf_architecture/execution/baseline_numbers.md`.
- Bench helper API on `App` documented.
- No production behaviour change.

## Deferred / explicitly not doing

- An interactive flamegraph capture. The existing
  `ANIE_PERF_TRACE` JSONL output already produces span-level
  attribution; criterion's mean + p95 is the gating signal we
  need. Flamegraph stays a manual diagnostic.
- Measuring keystroke→pixel latency including terminal drain.
  That's `crossterm`'s problem and varies wildly by terminal
  emulator; out of scope for a unit-bench harness.
