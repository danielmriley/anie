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

## Top-5 self-time functions (PerfSpan aggregation)

Captured by running the `resize_during_stream` scenario with
`ANIE_PERF_TRACE=1` and aggregating the resulting JSONL over
~119 frames. Sorted by total wall-time contribution to the
benchmark. These are the concrete targets for Phases 2-4.

| # | Function | File:line | Total time | Calls | Mean | p95 | p99 |
|---|----------|-----------|-----------:|------:|------:|-----:|-----:|
| 1 | `build_lines` | `crates/anie-tui/src/output.rs:431` | 14,095 ms | 119 | 118 ms | 121 ms | 128 ms |
| 2 | `block_lines` | `crates/anie-tui/src/output.rs:483` (call site) | 13,063 ms | 142,801 | 91 µs | 186 µs | 196 µs |
| 3 | `markdown_render` | `crates/anie-tui/src/markdown/layout.rs:26` | 12,743 ms | 71,400 | 178 µs | 187 µs | 209 µs |
| 4 | `wrap_spans` | `crates/anie-tui/src/markdown/layout.rs:848` | 455 ms | 499,800 | ~1 µs | 5 µs | 5 µs |
| 5 | `find_link_ranges` | `crates/anie-tui/src/output.rs:487` (call site) | 71 ms | 142,801 | ~0 µs | 1 µs | 1 µs |
| 6 | `paragraph_render` | `crates/anie-tui/src/output.rs:428` | 14 ms | 119 | 121 µs | 132 µs | 151 µs |

### What this data changes

The audit in `docs/tui_perf_architecture/README.md` had
`wrap_spans` as the #1 hot path. **The benchmark disagrees.**
In the realistic `resize_during_stream` workload:

- **`markdown_render` dominates at 178 µs per call × 71,400
  calls = 12,743 ms total** — ~91% of `build_lines` total
  time. Every cache miss triggers a full `pulldown-cmark` +
  syntect re-parse.
- **`wrap_spans` is only 455 ms total** — 36× cheaper than
  markdown. Not the bottleneck.
- **`paragraph_render` at 14 ms total** — confirms Plan 09
  viewport slicing did its job. Not a target.

### Priority flip

Phase 2 should target:

1. **Width-keyed cache that doesn't invalidate needlessly on
   same-width re-renders.** The current cache invalidates
   all blocks when width changes; if width oscillates (the
   `resize_during_stream` pattern), every frame is a
   cache-miss storm. Widths should be compared per-block,
   and re-rendering at the previous width should hit cache.
2. **Cache `markdown_render` output per `(text, width)`.**
   Currently re-parses on every miss. The finalized block
   text is immutable — the parse result can be memoized
   with a content hash. This is where the 91% win is.
3. **Arc-wrap the `LineCache` payload** (Plan 04 PR-B). Same
   as before; cheap constant-factor win.
4. **`wrap_spans` per-segment rewrite (Plan 04 PR-E).**
   Demoted from #1 to later in the plan because the
   benchmark says it's not where the time goes in the
   resize scenario. It may still matter in streaming; we'll
   measure again after #1-3 land.

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
