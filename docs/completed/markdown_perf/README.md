# Plan 09 — markdown rendering perf regression

> **Status (2026-04-25): superseded.** The instrumentation
> (PR-A) and the suspected fix targets (PR-B) were absorbed
> into `tui_perf_architecture/`:
>
> - `ANIE_PERF_TRACE` JSONL spans + criterion benches landed
>   under Plan 01 (`306e4e3`, `cd06e21`) — covers PR-A's
>   `build_lines` instrumentation and PR-C's benchmarks.
> - `Arc<Vec<Line>>` line cache + `wrap_spans` rewrite +
>   helper sweep landed under Plan 03/04 (`3fb113d`, `646de92`,
>   `c058f51`) — covers PR-B's "cheap clone via refcount"
>   and "fix wrap_spans cost" hypotheses.
> - Drain-batch coalescing of streaming deltas (`38eda8e`)
>   removed the per-token re-render cost the user felt as
>   slowness.
>
> See `tui_perf_architecture/execution/README.md` for the
> consolidated landing record. This doc is kept for the
> diagnostic reasoning but should not be used as a work
> source; pick from `tui_perf_architecture/` instead.

**Perf. The TUI felt slower after markdown enabled.**

User-reported: "the TUI feels slow again" after Plan 05 shipped
finalized-block markdown rendering. The block cache from
`tui_responsiveness` Plan 02 is supposed to memoize
`(content, width) -> Vec<Line>`, so markdown should only cost
once per block. Something isn't caching correctly, or the cost
even when cached is too high.

## Rationale

Before investigating, be honest about what's possible:

- **Block cache misses.** Streaming blocks bypass the cache
  (`block_has_animated_content` returns true). During streaming,
  each delta re-parses markdown. Plan 05 E.1 set
  `is_streaming || !markdown_enabled` → plain wrap so streaming
  should NOT pay markdown cost. Verify this actually fires on
  the streaming path.
- **Cache invalidation on width change.** Ratatui re-measures
  on every resize. If every redraw thinks the width changed,
  the cache re-computes every frame. Verify the cache key
  comparison is right.
- **syntect cost on finalized blocks.** Large code blocks cost
  more than we profiled. Should still be fine at 30 fps since
  it only runs on finalize, not per-frame, but worth measuring.
- **wrap_spans cell-by-cell.** `layout.rs::wrap_spans` does
  O(text_len) work on every block. For a 5000-char response,
  that's millions of char copies. The block cache memoizes
  this, but if the cache isn't hitting the ceiling stays high.

## Design

### Phase 1: measure

Add a render-time histogram to `OutputPane::build_lines`:
- Total render time.
- Per-block cache hit / miss counts.
- For each cache miss, the block's content length + how long
  `block_lines` took.

Gate the instrumentation behind `ANIE_PERF_TRACE=1` so it's
free in production. Emit as a single `tracing::info!` line per
redraw summarizing hits/misses/timings.

### Phase 2: fix whatever the profile shows

The likely offender determines the fix. Possibilities:

- **Cache never hits** — invalidation logic is wrong, always
  marking cache dirty. Fix: narrow the invalidation trigger.
- **Cache hits but per-hit cost is still high** — the Lines
  aren't being cloned cheaply (big String allocations). Fix:
  `Arc<Vec<Line>>` or similar so clone is a refcount bump.
- **Streaming block pays markdown cost** — the `is_streaming`
  check is wrong. Fix: audit `assistant_answer_lines` branch.
- **syntect init per-block** — syntax set loaded per-call
  instead of lazily-initialized. Fix: verify the `OnceLock`
  caches are shared.

### Phase 3: confirm the fix

Re-run the same workload with instrumentation, compare totals.
Unlike pi's TypeScript harness, Rust lets us benchmark cheaply.
Target: ≥50% reduction on a 20-message transcript redraw.

## Files to touch (speculative — confirmed after Phase 1)

| File | Change |
|------|--------|
| `crates/anie-tui/src/output.rs` | Instrumentation; likely a cache-invalidation fix. |
| `crates/anie-tui/src/markdown/layout.rs` | Possible cheaper wrap path. |
| `crates/anie-tui/src/markdown/syntax.rs` | Possible syntect init audit. |

## Phased PRs

### PR A — instrumentation

1. `ANIE_PERF_TRACE=1` env gate on a timing log in
   `OutputPane::build_lines`.
2. Emit: `redraw {blocks_total} {cache_hits} {cache_misses}
   {total_ms} {slowest_block_ms}`.
3. Ship + run against a long session locally to capture data.

### PR B — fix the hot spot

Depends on what Phase 1 finds. One concentrated fix, one
regression test that asserts the cache actually hits in the
case that was missing it.

### PR C — optional: micro-benchmarks

If Phase 1 shows a cross-cutting pattern, add `criterion`
benchmarks for `render_markdown` and `OutputPane::build_lines`
so future perf regressions are caught in CI.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | Cache hits when blocks unchanged between redraws. | `anie-tui::output` tests |
| 2 | Cache invalidates on content change but not on pure redraw. | same |
| 3 | Streaming block uses plain-wrap path, not markdown. | same |

## Risks

- **Perf "fixes" that make other things worse.** Any
  invalidation-narrowing change must preserve the
  block-changed-mid-render case. Easy to break scrolling.
- **Measurement without reproduction.** If the perf issue only
  shows on long transcripts with heavy markdown, we need a
  realistic fixture. User's current session is a good sample.

## Exit criteria

- [ ] Instrumentation PR merged.
- [ ] User re-runs their active session and confirms the TUI
      no longer feels slow.
- [ ] At least one concrete fix landed, with a regression test.
- [ ] Optional: criterion bench shows a measured improvement.

## Deferred

- **Incremental markdown parsing.** Re-parsing the full block
  on each finalize-cache-miss is cheaper than diffing and
  worth leaving alone unless Phase 1 shows it dominates.
