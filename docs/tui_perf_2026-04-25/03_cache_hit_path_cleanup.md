# 03 — Cache-hit path cleanup

## Rationale

Findings F-4, F-5, F-6, F-7. Four independent costs fire on
*every* paint regardless of dirty state — including urgent
keystroke paints that pass `reuse_flat_snapshot=true` and are
explicitly the cheap path:

- **F-4 / F-7**: `flat_lines[start..end].to_vec()` deep-clones
  the visible slice every paint. `flat_lines` stores owned
  `Line<'static>`, so this is a real Span/string copy, not a
  refcount bump. ~50–200 µs/frame.
- **F-5**: `has_animated_blocks()` walks every block on every
  `rebuild_flat_cache` call, even when the cache is valid.
  O(N) for a 600-block transcript.
- **F-6**: Status-bar `shorten_path()` calls
  `std::env::var("HOME")`, `replacen`, `split('/')`,
  `collect::<Vec<_>>()` per render. The cwd doesn't change per
  frame; the formatted string doesn't either.

Together these are the per-paint floor cost. The urgent paint
can't be cheaper than these no matter what else gets optimized.

## Design

### F-4 + F-7: borrow the visible slice instead of cloning

`OutputPane::render` builds a `Vec<Line<'static>>` of the
visible slice and passes it to `Paragraph::new(...)`
(`crates/anie-tui/src/output.rs:605-616`). ratatui's
`Paragraph::new` accepts anything `Into<Text<'a>>`, including
`&[Line<'a>]`. The slice clone is unnecessary.

**Plan:** drop `let visible = self.flat_lines[start..end].to_vec();`
and pass `&self.flat_lines[start..end]` (or, if the type bounds
require an owned `Text`, build it from a borrowed slice). The
exact ratatui call shape may need a small adapter — check
`Paragraph::new` and `Text::from(&[Line])` for the borrow form
in the version we use (currently 0.29). If borrow isn't
available, fall back to plan B below.

**Plan B (if ratatui requires owned Text):** change
`flat_lines: Vec<Line<'static>>` to
`flat_lines: Vec<Arc<Line<'static>>>`. The visible slice clone
becomes a refcount bump per Line. Per-block cache hits
(`output.rs:716-725`) push `Arc::clone(&line)` instead of deep
clones. Memory profile: same number of `Line`s alive, just
shared.

Plan A is preferred. Plan B is the fallback; it's a bigger
patch but addresses the same finding and also makes the
per-block cache extends cheap.

### F-5: cache `has_animated_blocks` invalidation

Track an `animated_block_count: usize` field on `OutputPane`.
Bump on `add_streaming_assistant` / `add_executing_tool`.
Decrement on `finalize_last_assistant` / tool finish. Then
`has_animated_blocks()` becomes `self.animated_block_count > 0`,
O(1).

The current implementation
(`crates/anie-tui/src/output.rs:624-625`) is:
```rust
fn has_animated_blocks(&self) -> bool {
    self.blocks.iter().any(block_has_animated_content)
}
```

The replacement maintains the same semantics — animated means
streaming or executing — but reads the count instead of
re-walking. Verify against the existing tests for streaming and
tool-execution state transitions.

### F-6: cache `shorten_path()` result

`shorten_path` is called from `render_status_bar` every frame.
The cwd it formats is on `App`, not changing per frame. Hold
the result on `App`:

```rust
struct App {
    cached_shortened_cwd: Option<(String /* raw cwd */, String /* shortened */)>,
    // ...
}
```

`render_status_bar` reads `cached_shortened_cwd`; if None or the
raw cwd doesn't match, recompute. The cwd changes when the user
runs `cd` inside a session (does that exist? — verify; if not,
the cache never invalidates after first render).

Same approach for the token formatters
(`format_tokens` calls in `render_status_bar`) if they show up
as a measurable cost in the new keystroke bench.

## Files to touch

- `crates/anie-tui/src/output.rs` — Plan A or Plan B for visible
  slice; `animated_block_count` field + maintenance in the
  `add_*` / `finalize_*` / tool helpers.
- `crates/anie-tui/src/app.rs` — `cached_shortened_cwd` on
  `App`; `render_status_bar` consults it.
- Tests across both files.

## Phased PRs

This plan is a single bundled PR. Each finding's fix is small
and they share invalidation reasoning (per-frame work that
should be paid only on state change). Bundling avoids three
small PRs against the same render path.

If reviewers prefer split, the natural decomposition is:
- PR 03a: visible slice borrow / Arc rewrap (F-4, F-7)
- PR 03b: animated count cache (F-5)
- PR 03c: status-bar cache (F-6)

In that order. PR 03a is the load-bearing one.

## Test plan

1. **`visible_slice_render_does_not_deep_clone_lines`** — if
   Plan A: snapshot test that the rendered output of a fixed
   transcript matches the existing snapshot byte-for-byte.
   If Plan B: assert `Arc::strong_count` is >= 2 after render
   (indicating the cache and the flat list share the same Arc).
2. **`animated_block_count_matches_walk`** — for a battery of
   transitions (add streaming, finalize, add tool, finish tool,
   nested), assert `self.animated_block_count == self.blocks
   .iter().filter(...).count()`. This is a property test on
   the maintenance code.
3. **`cached_cwd_renders_identically_to_uncached`** — render
   the status bar once cold, twice warm; assert both paints
   produce identical output bytes.
4. **`cached_cwd_invalidates_when_cwd_changes`** — synthetically
   mutate the App's cwd, render, observe the cache miss.
5. **PR 02 keystroke bench** — `keystroke_into_idle_app_600`
   improves measurably. Expected: 100–500 µs/frame reduction
   from the visible slice + status bar combined.

## Risks

- **ratatui borrow form.** Plan A depends on `Paragraph::new`
  accepting a borrowed `&[Line]`. If it doesn't in 0.29, Plan
  B is more code but the same ceiling improvement.
- **Animated-count drift.** Adding state means a place to forget
  to decrement. Mitigation: unit-level property test in #2
  above; debug-assert on every render that the count matches a
  walk (only under `cfg(debug_assertions)` so production stays
  fast).
- **cwd staleness.** If anie ever grows a way to change cwd
  mid-session (e.g., a `/cd` command), the cache must
  invalidate. Either make the cache inspection
  `if cached.0 != current_cwd` so it self-heals on mismatch,
  or expose a `cwd_changed()` hook.

## Exit criteria

- All four findings have an addressed fix or a documented
  reason not to.
- New keystroke-latency bench (PR 02) improves on
  `keystroke_into_idle_app_600`.
- Existing snapshot tests unchanged (if any).
- `cargo test --workspace` green; clippy clean.
- Manual smoke on a long transcript: idle scrolls feel cheaper.

## Deferred

- Replacing ratatui's `Paragraph` with a custom widget that
  accepts borrowed lines natively. Bigger surgery; do only if
  Plan A turns out to be more brittle than expected.
- Per-token-format caching beyond shortened cwd. The token
  numbers DO change per frame during streaming, so a cache
  there would invalidate constantly. Not worth it.
