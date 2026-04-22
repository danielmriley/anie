# Plan 01 — Baseline measurement + flamegraph

## Rationale

We shipped Plan 09 (viewport slicing) without a flamegraph. It
delivered a real 10× win at 600 blocks for the Paragraph render
specifically, but the audit here (see `README.md`) shows the
dominant remaining cost is upstream of Paragraph in
`build_lines` / `wrap_spans` — which Plan 09 didn't touch. The
user still reports sluggishness.

We do not ship another round of fixes without first having a
checked-in flamegraph and a reproducible stress scenario. Every
subsequent PR in this plan set is expected to cite a before/after
number relative to this baseline.

## Design

Three deliverables:

### 1. A reproducible stress harness

Extend the `build_lines_cached_stress` test
(`crates/anie-tui/src/output.rs`, currently `#[ignore]`) into a
small driver binary `anie-tui-bench` or a `criterion` benchmark
under `crates/anie-tui/benches/` that:

- Spins up an `OutputPane` populated with N synthetic blocks
  (N configurable: 100, 600, 2000).
- Each block mixes: plain text, a 40-line code fence with
  Rust syntax, a 5-paragraph markdown section, one table.
- Drives a simulated streaming response: appends 5-char deltas
  at 100 Hz for 30 seconds, forcing `build_lines` each call.
- Reports frame time p50/p95/p99 and total allocations
  (`jemalloc-ctl` or `dhat`).

This is the same shape as the existing stress test but with
tighter numbers and designed to be re-run PR-over-PR.

### 2. A flamegraph recipe

A one-line command we can run in the repo root:

```
ANIE_PERF_TRACE=1 cargo flamegraph --bench tui_render -- \
  --blocks 600 --stream-rate 100 --duration 30
```

Output `docs/tui_perf_architecture/execution/flamegraph_baseline.svg`
checked in (or a link to it if the repo prefers not to commit
SVGs — in that case keep the command and the summary text).

### 3. A perf-trace log schema

`ANIE_PERF_TRACE=1` already exists from Plan 09
(`render_debug.rs`). Extend it to emit timestamped spans for:

- `build_lines` entry/exit, with `blocks_total`,
  `cache_hits`, `cache_misses`, `streaming_block_present`.
- `markdown::render_markdown` entry/exit, with `text_len`.
- `wrap_spans` entry/exit, with `char_count`, `line_count`.
- `find_link_ranges` entry/exit.
- `Paragraph::render` entry/exit (wrap in a small local
  timer since ratatui's internals aren't instrumented).

Log format: one JSONL line per span, easy to parse with `jq`
for quick p50/p99. Write to `~/.anie/logs/perf.log.<pid>`.

## Files to touch

- `crates/anie-tui/benches/tui_render.rs` (new): criterion
  benchmark driving the stress scenario.
- `crates/anie-tui/Cargo.toml`: `[dev-dependencies] criterion =
  "0.5"`, `[[bench]] name = "tui_render" harness = false`.
- `crates/anie-tui/src/render_debug.rs`: extend the
  `PerfTrace` struct with new span kinds, emit JSONL.
- `crates/anie-tui/src/output.rs`: add perf spans in
  `build_lines`, `block_lines`, and at the Paragraph call site.
- `crates/anie-tui/src/markdown/layout.rs`: perf spans in
  `wrap_spans`, `render_markdown`.
- `crates/anie-tui/src/output.rs`: perf span in
  `find_link_ranges`.
- `docs/tui_perf_architecture/execution/README.md` (new):
  baseline numbers + flamegraph link.

## Phased PRs

### PR-A: perf-trace JSONL + spans

Add the extended perf-trace with span entry/exit and JSONL
output. No new benchmark yet. Verify manually by running the
TUI with `ANIE_PERF_TRACE=1` and checking `perf.log.<pid>`.

- Exit: spans land in the log file, parseable by `jq`.

### PR-B: criterion benchmark

Add `crates/anie-tui/benches/tui_render.rs` with three scenarios:
`scroll_static_600`, `stream_into_static_600`,
`resize_during_stream`. Report p50/p95/p99 frame time.

- Exit: `cargo bench -p anie-tui` runs to completion, output
  captured in `execution/baseline_numbers.md`.

### PR-C: flamegraph capture + commit

Run the benchmark under `cargo flamegraph`; capture SVG to
`execution/flamegraph_baseline.svg`. Write up a one-page
summary (`execution/README.md`) identifying the top 5 hot
functions by self-time.

- Exit: the top-5 list matches or refines the README.md
  diagnosis. If it doesn't, the plan set's ordering is
  re-evaluated before landing Plan 02+.

## Test plan

- The new benchmark compiles and runs headlessly (no TTY
  needed). Use a `TestBackend` like the existing stress test.
- Perf-trace emits nothing when `ANIE_PERF_TRACE` is unset
  (smoke test).
- JSONL is valid and parseable: one `jq '.kind' perf.log.*`
  run produces no errors.

## Risks

- **Benchmark noise.** TUI benchmarks are notoriously flaky.
  Mitigation: use `TestBackend`, pin the scenario's width/height,
  run 10× in CI and report median. Accept ±15% variance.
- **dhat / jemalloc overhead.** Memory tracking slows the
  benchmark meaningfully. Keep it as a separate invocation
  gated behind `--features perf-mem`.
- **Perf-trace itself costs measurable time.** Keep span
  emission cheap (format into a thread-local buffer, flush on
  drop). Gate writes behind `ANIE_PERF_TRACE=1` env check at
  the top of each span.

## Exit criteria

- [ ] `crates/anie-tui/benches/tui_render.rs` exists and runs
      cleanly via `cargo bench -p anie-tui`.
- [ ] `execution/baseline_numbers.md` captures p50/p95/p99
      for the three scenarios.
- [ ] `execution/flamegraph_baseline.svg` (or a referenced
      artifact) exists.
- [ ] `execution/README.md` lists the top-5 self-time
      functions with file:line and rough percentages.
- [ ] `ANIE_PERF_TRACE=1 anie` writes JSONL spans to
      `~/.anie/logs/perf.log.<pid>`, parseable by `jq`.

## Deferred

- **Continuous perf tracking in CI.** Nice to have but noisy
  without dedicated hardware. Not blocking.
- **Automated regression gates.** Once the benchmark exists
  we can add a GitHub Actions check, but let it run locally
  for a few weeks first to learn the variance.
