# TUI responsiveness — execution

Tracker for the three PRs. Update status inline as each lands.

## Baseline

Partial fix in `df2d2c4` (drain agent events + dirty flag) is
already on `feat/provider-compat-blob`. These PRs stack on top.

| PR | Status | Commit |
|----|--------|--------|
| [PR 1 — render scheduling + 30 fps cap](../01_render_scheduling.md) | landed | `7d26315` |
| [PR 2 — per-block line cache](../02_output_pane_cache.md) | landed | `bfd2628` |
| [PR 3 — debug instrumentation](../03_debug_instrumentation.md) | landed | `ced1442` |

## Ordering

PR 1 first: cheapest change, obvious correctness, bounds the
rate cost. Validates end-to-end before we touch the hotter
hot-path in PR 2.

PR 2 second: structural fix for per-frame cost. Most of the work
and most of the user-visible win.

PR 3 third: instrumentation against the already-fixed baseline
so the first real log lines show the *new* redraw profile, not
the old one.

## Gate per PR

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Plus a manual smoke: long agent run (e.g. the Nemotron session
that surfaced the original bug), keystrokes during streaming,
scroll through history. No input lag.

## Follow-up: bounded agent-event drain

`docs/code_review_2026-04-24/09_tui_event_drain_bounds.md`
added an explicit cap to the agent-event drain path. The cap is
256 events per frame, including the first awaited event, because
interactive mode uses `mpsc::channel(256)` and a saturated
channel burst drained 256 events in one frame before the bound.

Benchmark smoke after the cap:

```text
cargo bench -p anie-tui --bench tui_render -- --warm-up-time 1 --measurement-time 3
scroll_static_600:       [312.31 us 312.47 us 312.62 us]
stream_into_static_600:  [2.0288 ms 2.0341 ms 2.0413 ms]
resize_during_stream:    [96.466 ms 96.609 ms 96.776 ms]
```

No tuning change was needed: the cap matches the realistic
saturated burst size while preventing producer refills from
extending a single frame's drain indefinitely.
