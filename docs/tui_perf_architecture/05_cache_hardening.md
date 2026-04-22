# Plan 05 — Cache hardening (resize + link-map)

## Rationale

After Plan 03 (Arc-wrap + wrap rewrite) and Plan 04 (streaming
coalescing), two edge cases remain visible in the audit:

1. **Resize invalidates all block caches at once**
   (`crates/anie-tui/src/output.rs:174–178`). Every resize
   event clears `LineCache` for every block. During a real
   resize (drag), the terminal emits a burst of 10–30 resize
   events. Each subsequent frame recomputes lines for all
   600 blocks. On the first post-resize frame the user sees a
   noticeable stutter.

2. **Link-map rebuilt for all blocks every frame**
   (`output.rs:486–488`, added in Plan 10
   `tui_hyperlinks/`). The post-scan for clickable URL ranges
   runs for every block on every draw, including cached ones
   that haven't changed. This is a separate loop from the
   line cache and doesn't benefit from Arc-wrapping.

Both are O(blocks) per frame and dwarf the median-case work
once Plan 03 lands.

Plan 05 turns each into O(changed blocks) per frame.

## Design

### 5.1 Per-block width cache key + resize debounce

Today: `LineCache` is width-agnostic; when width changes,
`invalidate_all_caches` is called. Replace with a cache keyed
on `(block_id, width, theme_id)`. On width change, the old
entries stay but a cache miss at the new width triggers a
rebuild for that block.

This is **not** keeping N caches per block — the cache
replaces its entry whenever a different (width, theme_id) is
requested. So memory is still ~1 entry per block.

Second change: debounce resize. During a drag the terminal
fires many `Resize` events within ~100 ms. Track the last
resize timestamp in `app.rs`; if another resize arrives
within `RESIZE_DEBOUNCE_MS` (50 ms), mark `dirty` but don't
reset any caches yet. Only when resize quiesces do we proceed
to a full render. The render will still need to rebuild
every block's lines, but it happens **once** at the new
width, not on every intermediate size.

### 5.2 Per-block link-map

Today: `find_link_ranges` walks all rendered lines each
frame. Turn it into a parallel cache:

```rust
struct LinkCache {
    block_id: BlockId,
    width: u16,
    theme_id: ThemeId,
    ranges: Arc<Vec<LinkRange>>,
}
```

Compute link ranges inside `block_lines` when a block is
(re)rendered, store alongside the `LineCache`, and use the
cached ranges on hit. The render loop concatenates link-range
caches for visible blocks to build the per-frame link map.

### 5.3 Coalesce line cache + link cache into one entry

Practical refactor: one `BlockRender` struct holds both:

```rust
struct BlockRender {
    width: u16,
    theme_id: ThemeId,
    lines: Arc<Vec<Line<'static>>>,   // from Plan 03
    links: Arc<Vec<LinkRange>>,       // new
}
```

One invalidation path, one (width, theme) key, one Arc to
bump. Cleaner than two parallel caches.

## Files to touch

- `crates/anie-tui/src/output.rs`:
  - `LineCache` type → `BlockRender` type (merges line +
    link caches).
  - `build_lines` uses `BlockRender` on hit, skips
    `find_link_ranges` when hitting.
  - `invalidate_all_caches`, `invalidate_last` — update for
    new shape.
  - Width/theme_id tracking on each entry.
- `crates/anie-tui/src/app.rs`: resize debounce logic in
  the main `tokio::select!` loop.
- `crates/anie-tui/src/markdown/mod.rs`: `find_link_ranges`
  already operates on `&[Line]`; no change needed beyond
  where it's called from.
- `crates/anie-tui/src/tests.rs`: new tests for resize debounce
  and per-block link caching.

## Phased PRs

### PR-A: merge line + link caches into `BlockRender`

Refactor only. No behavior change. Goal: one invalidation
path, one entry per block.

- Exit: `cargo test --workspace` green; no benchmark change
  expected.

### PR-B: per-block link cache

Skip `find_link_ranges` for blocks that hit the `BlockRender`
cache. Only recompute for cache-miss blocks.

- Exit: flamegraph shows `find_link_ranges` self-time drops
  proportionally to cache hit rate (≥80% for a static
  transcript).

### PR-C: resize debounce + width/theme keyed cache

Debounce window + switch `invalidate_all_caches` on resize
to a no-op (cache entries at the old width will simply miss
and re-render at the new width on the next build_lines).

- Exit: dragging the terminal resize across 10 widths takes
  no longer than a single resize to the final width. Stutter
  on drag-release is eliminated.

## Test plan

- **Behavior parity.** All current tests pass. Adding a new
  per-frame `find_link_ranges` skip must not break mouse
  click-to-open behavior — regression test: click on a URL
  after scrolling to a cached block, verify the URL opens.
- **Resize debounce.** A test feeds 10 `Resize(w, h)` events
  with 10 ms spacing; assert only one `build_lines` call
  fires (counted via the Plan 01 perf span).
- **Width-keyed cache invalidation.** Width change from 80
  to 120 triggers a miss → rebuild for all blocks at 120.
  Next frame at 120 is cache-hit. Switching back to 80 is
  also a cache-miss (not retained at both widths).
- **Theme change invalidation.** Toggling dark/light causes
  a cache-miss (because `theme_id` differs).

## Risks

- **Cache entry doesn't match current width.** If invalidation
  on resize is weakened, stale lines could render at the old
  width. Mitigation: cache lookup compares `(width, theme_id)`
  and falls through to recompute on mismatch — this is the
  correct behavior, not a bug.
- **Resize debounce feels laggy.** 50 ms is plausibly below
  perception threshold for a drag. If users notice, shrink
  to 30 ms. If not, consider 75 ms.
- **Link regression.** Merging line + link caches means a
  bug in one affects both. Mitigate with per-function unit
  tests on `BlockRender` construction.

## Exit criteria

- [ ] `BlockRender` struct replaces `LineCache`.
- [ ] `find_link_ranges` runs only for cache-miss blocks in
      `build_lines`.
- [ ] Resize drag from 80→200 cols feels single-event: no
      per-intermediate-width stutter.
- [ ] Width-keyed cache: changing width invalidates
      correctly; switching back also invalidates (no stale
      lines).
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] Flamegraph: `find_link_ranges` self-time ≤ 1% on
      `scroll_static_600` bench.

## Deferred

- **Keeping entries at multiple widths per block.** We don't
  switch widths often enough mid-session for this to matter.
  One entry per block is plenty.
- **Link-map hit testing data structure.** Currently linear
  scan via `url_at_terminal_position`. At 600 blocks × few
  links per block, linear is fine. Revisit only if clicks
  become visibly slow.
- **Theme-id strong typing.** A `u32` hash of the active
  theme is enough for cache keying. No new type.
