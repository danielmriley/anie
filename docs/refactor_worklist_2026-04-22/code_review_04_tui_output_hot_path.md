# Plan 04 — TUI output hot path

**Findings covered:** #3, #4, #5, #39, #47, #48, #52

This plan operationalizes the OutputPane-related findings from the
performance review while explicitly reusing the existing
`docs/tui_responsiveness/` work where it already covers the same
ground.

## Rationale

The review found that the TUI still has concentrated cost in three
places:

1. wrapping text/spans per frame with per-char allocations (**#3,
   #4**)
2. cloning cached lines both on cache read and cache write (**#5,
   #47**)
3. a handful of repeated helper allocations in the render path
   (**#39, #48, #52**)

The existing `docs/tui_responsiveness/` plan already covers:

- render scheduling / 30 FPS cap
- per-block caching in `OutputPane`

This plan does **not** replace that work. It tightens it so the
cache actually removes both clone sites, and it adds the remaining
review findings that were outside the earlier responsiveness plan.

## Design

### 1. Reuse the existing render-scheduling plan

If `docs/tui_responsiveness/01_render_scheduling.md` has not landed,
land it first. This plan assumes render rate is already bounded; the
remaining work is to reduce the cost of each frame.

### 2. Upgrade the block cache to remove both clone sites

The current review findings show two clone points:

- **read-side:** `cached.lines.iter().cloned()`
- **write-side:** `lines: computed.clone()`

The cache should store `Arc<Vec<Line<'static>>>`, not a bare
`Vec<Line<'static>>`.

That lets the cache own a single backing allocation while the render
path cheaply shares it. If the current `OutputPane::to_lines`
signature makes this awkward, introduce a small internal helper type
rather than keeping the clone-heavy cache.

### 3. Rewrite wrap helpers to allocate per segment, not per char

`wrap_plain_text` and `wrap_spans` should:

- operate on segment boundaries
- build `String` output once per visible segment
- never allocate one `String` per character

Important constraints from the review:

- preserve current `chars().count()` semantics
- do **not** silently introduce Unicode display-width logic here
- keep the `wrap_spans` empty-slice guard (`!slice.is_empty()`)

### 4. Sweep render-path helper allocation patterns

After the structural cache/wrap work:

- use `&'static str` from `spinner.tick()` directly (**#39**)
- replace two-`Vec` chain construction with direct container build
  (**#48**)
- replace collect-then-join transcript helpers in `app.rs`
  (`tool_result_body`, `tool_result_message_body`) with direct-buffer
  builds (**#52**)

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-tui/src/output.rs` | cache storage, wrap helper rewrites, small render-path helpers |
| `crates/anie-tui/src/app.rs` | spinner frame borrow and transcript helper cleanup |
| `docs/tui_responsiveness/` | Only if the older plan docs need cross-links / update notes after landing |

## Phased PRs

### PR A — land or align render scheduling

1. Reuse `docs/tui_responsiveness/01_render_scheduling.md`.
2. Do not mix the scheduling change with wrapping/cache rewrites.
3. If already landed, mark this PR as skipped in the execution
   tracker.

### PR B — Arc-backed `OutputPane` cache storage

1. Update `LineCache` to hold `Arc<Vec<Line<'static>>>`.
2. Remove the write-side `computed.clone()` path.
3. Keep animated blocks uncached or separately handled, as the
   existing responsiveness plan already discusses.

### PR C — cache read-side cleanup + invalidation audit

1. Remove the read-side deep-clone path.
2. Audit invalidation and animated-block handling in the new cache
   shape.
3. Keep this separate from the wrap-helper rewrite so visual failures
   are easier to isolate.

### PR D — `wrap_plain_text` rewrite

1. Rewrite `wrap_plain_text`.
2. Keep char-count semantics unchanged.

### PR E — `wrap_spans` rewrite

1. Rewrite `wrap_spans`.
2. Add the empty-slice guard from the report.
3. Keep char-count semantics unchanged.

### PR F — render-helper cleanup

1. `spinner.tick()` stays borrowed.
2. Fix the two-span vector construction in `output.rs`.
3. Replace the two collect-then-join tool-result helpers in `app.rs`.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | Existing `OutputPane` cache tests updated for `Arc<Vec<Line>>` shape | `crates/anie-tui/src/output.rs` tests |
| 2 | `wrap_plain_text_matches_existing_layout_without_per_char_allocation_regression` | same |
| 3 | `wrap_spans_does_not_emit_empty_span_at_width_boundary` | same |
| 4 | `streaming_spinner_blocks_do_not_reuse_stale_cached_lines` | same |
| 5 | Transcript/tool-result helper output matches current snapshots | `app.rs` / TUI tests |

## Risks

- **Lifetime / ownership complexity in cache storage:** use a small
  internal helper rather than forcing `Arc` through public APIs.
- **Visual regressions:** wrapping changes are easy to get "mostly
  right" while still shifting line boundaries. Snapshot tests are
  important here.
- **Display-width temptation:** keep Unicode-width correctness
  explicitly deferred.

## Exit criteria

- [ ] OutputPane no longer deep-clones cached lines on both read and
      write.
- [ ] `wrap_plain_text` and `wrap_spans` no longer allocate per
      character.
- [ ] `spinner.tick()` no longer allocates every frame.
- [ ] Tool-result transcript helpers no longer build intermediate
      `Vec<&str>` values.
- [ ] No visual snapshot regressions in the TUI tests.

## Deferred

- Unicode display-width correctness.
- Any broader TUI renderer redesign beyond the scoped `OutputPane`
  cleanup here.
