# Phase 0 baseline numbers

Captured: 2026-04-22 via `cargo bench -p anie-tui --bench
tui_render`. Release build, criterion 0.5, TestBackend
120×40 viewport, 100 samples per scenario.

## Scenario results

| Scenario | Mean frame time | Criterion CI |
|----------|----------------:|-------------:|
| `scroll_static_600` | **3.20 ms** | [3.1975 ms, 3.2122 ms] |
| `stream_into_static_600` | **3.40 ms** | [3.3882 ms, 3.4048 ms] |
| `resize_during_stream` | **104.00 ms** | [103.85 ms, 104.17 ms] |

Outliers at each: 5%, 2%, 5% respectively — acceptable
within the ±15% variance budget stated in
`docs/refactor_worklist_2026-04-22.md`.

## Observations

- **`scroll_static_600` at 3.2 ms** — cache-hit steady state
  for a 600-block transcript is well under the 33 ms/30 fps
  budget. The existing Plan 09 viewport slicing + block
  cache holds up here. This is NOT where the sluggishness
  is.
- **`stream_into_static_600` at 3.4 ms** — only ~200 µs
  worse than scroll. So the marginal cost of appending a
  5-char delta and re-wrapping the streaming block is
  small. The cost scales with streaming-block text length,
  which this benchmark keeps short — re-run once Plan 04
  lands to see the effect on longer streams.
- **`resize_during_stream` at 104 ms** — **this is the
  sluggishness**. Alternating widths forces `invalidate_all_caches`
  (or a width-mismatched miss in `build_lines`) across all
  600 blocks per resize, and each rebuild pays full
  markdown-parse + syntect highlight + wrap cost. This is
  the primary target for Phase 2 (hot-path rewrite) and
  Phase 4 (cache hardening with width-keyed invalidation).

## Follow-up targets (derived from the above)

After Phase 2 (`code_review 04` A-F) lands:
- `scroll_static_600` allocations/frame drop ≥ 50%.
- `stream_into_static_600` p50 drops ≥ 30%.

After Phase 3 + Phase 4 land:
- `stream_into_static_600` drops ≥ 50% more.
- `resize_during_stream` should drop by an order of
  magnitude — widths stop triggering global invalidation,
  and per-block rebuilds only happen for the blocks
  actually visible or recently visible.

## Flamegraph

**Deferred.** `cargo flamegraph` and `perf record` require
`kernel.perf_event_paranoid <= 2`; this host is at 4. To
capture:

```bash
# one-time, as root (or set persistently in /etc/sysctl.d/):
sudo sysctl kernel.perf_event_paranoid=1
# capture (from repo root):
cargo install flamegraph
cargo flamegraph --bench tui_render --bench-args --bench \
  -- --profile-time 10 resize_during_stream
```

Output: `flamegraph.svg` in the repo root — move to
`docs/tui_perf_architecture/execution/flamegraph_baseline.svg`
or link.

Alternatives that don't require paranoid-level access:
- **`samply`** (user-level sampling profiler, no sudo):
  ```bash
  cargo install samply
  samply record -- target/release/deps/tui_render-* --bench \
    --profile-time 10 resize_during_stream
  ```
- **`pprof-rs`** crate integrated into the bench binary
  (gated behind a feature flag). Would need a small source
  change to opt in; noted as a follow-up if the sysctl
  tweak isn't available on the target machine.

The JSONL perf spans from PR-A are a partial substitute:
run any scenario with `ANIE_PERF_TRACE=1` and query
`~/.anie/logs/perf.log.<pid>` with `jq` for per-function
cost distributions. Not a full flamegraph (no call stacks)
but catches 80% of "which function is hot" questions.

## How to re-run

```bash
# full run (~90 seconds)
cargo bench -p anie-tui --bench tui_render

# quick smoke (shorter wall time, more noise)
cargo bench -p anie-tui --bench tui_render -- \
  --warm-up-time 1 --measurement-time 3

# compare across a change
cargo bench -p anie-tui --bench tui_render -- --save-baseline before
# ... land PRs ...
cargo bench -p anie-tui --bench tui_render -- --baseline before
```

The criterion HTML report at `target/criterion/report/index.html`
has the full distribution + percentiles if you want them.
