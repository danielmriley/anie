# 01 — Input pane: stop running `layout_lines` twice per keystroke

## Rationale

Finding F-1. Every keystroke paint invokes `InputPane::layout_lines`
twice: once via `preferred_height` from `App::render_with_mode`
(`crates/anie-tui/src/app.rs:531-533`), then again from
`InputPane::render` (`crates/anie-tui/src/input.rs:287`).

`layout_lines` (`input.rs:500-554`) walks `self.content.char_indices()`
end-to-end and rebuilds a `Vec<String>`. For a 200-char buffer,
that's ~400 char operations + 2 fresh `Vec` allocations per
keypress — work that happens *on top of* the actual paint.

This is the strongest candidate for "subtle but definite" residual
input lag. The output pane's urgent path is already correct (it
reuses the flat snapshot, skips DECSET 2026 sync wrap, and doesn't
walk transcript state). The cost lives one widget over.

## Design

Move the layout result into `InputPane` itself, keyed by
`(width, content_revision)`. A revision counter increments on
every mutation (insert / delete / paste / cursor move that affects
displayed cursor position). Both `preferred_height` and `render`
read from the cached layout when the key matches, recompute
otherwise.

### Sketch

```rust
struct InputPane {
    content: String,
    cursor: usize,
    revision: u64,
    cached_layout: Option<CachedLayout>,
    // ... existing fields
}

struct CachedLayout {
    width: u16,
    revision: u64,
    lines: Vec<String>,
    cursor: (u16, u16),
}

impl InputPane {
    fn layout(&mut self, width: u16) -> &CachedLayout {
        let key = (width, self.revision);
        if !matches!(&self.cached_layout, Some(c) if (c.width, c.revision) == key) {
            let (lines, cursor) = self.layout_lines_uncached(width);
            self.cached_layout = Some(CachedLayout { width, revision: self.revision, lines, cursor });
        }
        self.cached_layout.as_ref().unwrap()
    }

    pub fn preferred_height(&mut self, width: u16) -> u16 { ... }
    pub fn render(&mut self, area: Rect, buf: &mut Buffer) -> Position { ... }
}
```

`preferred_height` becomes `&mut self` (it can update the cache).
That's a small API ripple — `render_with_mode` already has
`&mut self` on `App`, and `App.input_pane` is owned, so the
borrow chain is fine.

### Why a revision counter, not `Cow`-style "was content
mutated"

The cache must invalidate on cursor moves too (cursor position
is part of the layout output). Tracking just content mutations
would still recompute on arrow keys when the layout result is
visually unchanged except for the cursor cell. A revision
counter that bumps on any change covered by the layout output
keeps the predicate simple and correct.

### What does NOT change

- `layout_lines_uncached` is the renamed current `layout_lines`.
  Same algorithm. We're not making it cheaper; we're making it
  run once.
- The cache is cleared whenever the content or cursor mutates;
  no tricky partial-update logic.
- Width changes (resize) invalidate naturally via the `width` part
  of the key.

## Files to touch

- `crates/anie-tui/src/input.rs` — add `revision: u64`,
  `cached_layout: Option<CachedLayout>`, bump in every mutating
  method, change `layout_lines` → `layout_lines_uncached`, add
  `layout(&mut self, width: u16) -> &CachedLayout`. Adjust
  `preferred_height` and `render` to use it.
- `crates/anie-tui/src/app.rs` — `preferred_height` call site at
  `:531-533` switches to `&mut self.input_pane`. Already holds
  `&mut self` on `App` so the borrow is straightforward.
- Tests in `crates/anie-tui/src/input.rs` (existing module) —
  add a regression test that the same `(width, content)` pair
  computes layout once.

## Phased PRs

Single PR. <3 files, narrow scope, behaviour-preserving.

## Test plan

1. **`layout_cached_for_unchanged_content_and_width`** — call
   `preferred_height(120)` and `layout(120)`; assert the second
   call is served from the cache (instrument with a debug
   counter behind `#[cfg(test)]` on `InputPane`).
2. **`layout_invalidates_on_insert`** — insert a char, call
   `layout` again, assert recompute.
3. **`layout_invalidates_on_cursor_move`** — `move_cursor_left()`,
   call `layout`, assert recompute (cursor visual position
   changes).
4. **`layout_invalidates_on_width_change`** — call at width 80,
   then width 120, assert recompute.
5. **Existing tests must still pass** — `layout_lines` was
   tested via `preferred_height` and `render`; their behavior
   is unchanged.

## Risks

- **`&mut self` propagation.** `preferred_height` becomes
  `&mut self`. `App::render_with_mode` already takes
  `&mut self`, so the call site is fine, but check whether any
  other call site (tests, an overlay) expects `&self` —
  `grep preferred_height` first.
- **Cache invalidation completeness.** If a future change adds
  a method that mutates `content` or `cursor` without bumping
  `revision`, the cache goes stale. Mitigation: a
  `bump_revision()` helper called at the top of every mutating
  method, plus a debug-assert in `layout` that the cached
  cursor matches what we'd recompute (only run under `cfg(test)`
  or `cfg(debug_assertions)`).

## Exit criteria

- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- New keystroke-latency bench (PR 02) shows a measurable
  improvement on a multi-line buffer.
- Manual smoke: type into a long pre-filled buffer, no perceived
  lag.

## Deferred / explicitly not doing

- Replacing the `Vec<String>` layout output with a more
  efficient representation (flat string + line offsets). Worth
  it later if profile data shows the layout itself dominates
  after dedupe; not part of this PR.
- Sharing the cache between `InputPane` and the autocomplete
  context parser (Finding F-2). Different invalidation
  triggers; collapsing is a separate refactor.
