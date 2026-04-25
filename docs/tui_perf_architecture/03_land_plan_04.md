# Plan 03 — Land Plan 04 hot-path fixes

## Rationale

The existing `docs/code_review_performance_2026-04-21/04_tui_output_hot_path.md`
describes a full set of per-frame fixes that are **fully scoped
but not shipped**. The audit in this folder (see `README.md`)
confirms those same fixes are the dominant remaining cost:

- `Vec<Line<'static>>` cache clones on hit and on write.
- `wrap_spans` flattening every span into a `Vec<(char, Style)>`
  with one entry per character.
- Allocation churn in spinner-frame construction and thinking-
  gutter prefix rendering.

Before we design new architecture, we cash in the tickets that
are already designed. This plan is the *execution half* of
Plan 04; the design half lives under
`docs/code_review_performance_2026-04-21/` and is not
re-litigated here. Where the audit's file:line numbers diverge
from Plan 04's, this plan trusts the audit (fresher).

## Design

Three structural changes, each landing as its own PR:

### 3.1 Arc-wrap the `LineCache` payload

`LineCache` currently stores `Vec<Line<'static>>`. Every cache
hit clones the `Vec` and every `Span` inside. Switch to
`Arc<Vec<Line<'static>>>` and return `Arc::clone` on hit. The
output-builder step extends from an iterator of `Line`
references (via `(*arc).iter().cloned()`) or, better, concats
the Arcs into a `Vec<Arc<Vec<Line>>>` and flattens at
Paragraph-render time.

The precise shape (flattening vs single-vec) falls out of
Plan 04's design — follow that design doc when implementing.

### 3.2 Rewrite `wrap_spans` to allocate per-segment

Current code (`crates/anie-tui/src/markdown/layout.rs:~839–899`):

```rust
let mut cells: Vec<(char, Style)> = Vec::new();
for span in spans {
    for ch in text.chars() {
        cells.push((ch, style));  // allocation per char
    }
}
```

Rewrite to iterate spans and break-at-width within each span's
text without flattening. Produce `Line<'static>` with
contiguous `Span` slices per wrap boundary. Keep behavior
identical for display-width purposes (the existing
`chars().count()` semantics — a separate Unicode-width change
is explicitly deferred per Plan 04's own deferral list).

Target: same wrap output, one allocation per output line
instead of one allocation per input character.

### 3.3 Helper-allocation sweep

From Plan 04 PR-F and the audit:

- Spinner tick construction (`output.rs:~326`) — precompute
  the spinner frame strings once, index them instead of
  reformatting per frame.
- Thinking-gutter prefix (`output.rs:~660`) — stop
  `.to_string()`-ing the prefix per gutter line; use a
  shared `&'static str`.
- Tool-call index scan (`output.rs:~373–383`) — O(N) linear
  scan per update. Track a tail-block index.

Each is a small, independent patch.

## Files to touch

- `crates/anie-tui/src/output.rs` (the `LineCache` type,
  `build_lines`, `block_lines`, spinner, thinking gutter,
  tool-call tracker).
- `crates/anie-tui/src/markdown/layout.rs` (`wrap_spans`,
  `cells_to_line`).
- Cache-invalidation call sites: `append_to_last_assistant`,
  `invalidate_last`, `invalidate_all_caches` — verify they
  release the Arc (drop the old Arc; a new Arc is created on
  next cache miss).
- Tests: `crates/anie-tui/src/output.rs#[cfg(test)] mod tests`
  and `crates/anie-tui/src/markdown/layout.rs#[cfg(test)] mod
  tests` — add cases for the rewrite where existing tests
  don't cover allocation counts.

## Phased PRs

Each PR lands with a before/after number from the Plan 01
benchmark.

### PR-A: Arc-wrap LineCache

- Implement `Arc<Vec<Line<'static>>>` on the cache.
- Change the extend path to clone the Arc, not the Vec.
- Exit: `scroll_static_600` benchmark drops in allocated
  bytes by >50%; frame p50 drops.

### PR-B: rewrite wrap_spans per-segment

- Replace the `Vec<(char, Style)>` flatten with per-span
  iteration.
- Property test: for 100 randomized span sequences at widths
  10..200, new output equals old output byte-for-byte.
- Exit: `stream_into_static_600` p50 drops by ≥30%.

### PR-C: helper sweep

- Spinner, thinking gutter, tool-call index — individual
  small patches.
- Exit: flamegraph no longer shows any of these three as
  top-20 self-time functions.

## Test plan

- **Existing behavior preserved.** Every existing test in
  `output.rs` and `markdown/layout.rs` passes unchanged.
- **Property test for wrap_spans.** A randomized test
  generates 100 `Vec<Span>` of mixed styles and widths; the
  new `wrap_spans` produces bit-identical output to a
  reference copy of the old one (kept in a `#[cfg(test)]`
  module as `wrap_spans_reference`).
- **Allocation regression test.** Use `dhat` in
  `#[cfg(feature = "perf-mem")]` to assert allocations per
  render frame are below a fixed cap for a 600-block static
  transcript.
- **Cache invalidation contract.** Existing invalidation
  tests must continue to pass. Add one new test: after
  `invalidate_last`, the previous Arc is dropped (use a
  `Weak` to verify).

## Risks

- **Subtle wrap-output divergence.** Easiest way to break is
  an off-by-one in the per-segment rewrite. Mitigation:
  property test (above) plus byte-for-byte regression test on
  a 50-block golden transcript.
- **Arc cycle / leak.** `Arc<Vec<Line>>` can't cycle, but
  forgetting to drop old Arcs on invalidation would pin
  memory. Weak-pointer test covers this.
- **Thread-safety bound** — `Line<'static>: Send + Sync` for
  Arc-wrapping. `Line<'static>` is `Span<'static>` which is
  `Send + Sync` already. No bound change needed, but confirm
  at compile time.
- **Cache-write path clones on construction**, not just
  readers. The write path must also move, not clone, into
  the Arc. Easy to get wrong.

## Exit criteria

- [ ] All three PRs landed.
- [ ] `scroll_static_600` bench: allocations/frame drop
      ≥50% vs Plan 01 baseline.
- [ ] `stream_into_static_600` bench: frame p50 drops ≥30%
      vs Plan 01 baseline.
- [ ] Flamegraph re-capture: no Plan 04-scope function in
      top-20 self-time.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] Plan 04 (`docs/code_review_performance_2026-04-21/04_tui_output_hot_path.md`)
      marked "landed" in its execution tracker.

## Deferred

- **Unicode display-width rewrite.** Plan 04 defers this
  explicitly; this plan inherits the deferral.
- **Line-cache eviction strategy.** Currently the cache
  grows unbounded with the transcript. At ~600 blocks of
  ~40 lines each, memory is <5 MB per session, so bounded
  growth is cheap. Revisit if sessions routinely exceed 10k
  blocks.
