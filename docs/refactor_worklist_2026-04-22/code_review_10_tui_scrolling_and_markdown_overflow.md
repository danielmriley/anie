# code_review_performance_2026-04-21 / 10: TUI scrolling + markdown overflow

## Rationale

There are two separate problems in the current TUI, and they should not
be solved with the same mechanism.

### 1. Vertical transcript scrolling has state, but no app scrollbar

`OutputPane` already tracks vertical scroll state:

- `scroll_offset`
- `last_total_lines`
- `last_viewport_height`
- `max_scroll()`
  (`crates/anie-tui/src/output.rs:102-109`, `379-471`)

But render-time output is still just:

```rust
Paragraph::new(lines).scroll((self.scroll_offset, 0)).render(area, buf);
```

(`crates/anie-tui/src/output.rs:391-393`)

So anie currently has:

- wheel scrolling
- PageUp / PageDown / Home / End scrolling
- **no rendered scrollbar thumb**
- **no mouse click / drag path**

The mouse handler confirms that only the wheel is wired today
(`crates/anie-tui/src/app.rs:703-708`).

This also explains the terminal-emulator scrollbar behavior: the app is
running in an alternate-screen TUI, so the side scrollbar the user sees
belongs to the terminal, not to anie's transcript state. Dragging that
bar is not a viable in-app interaction target. If we want draggable
scrolling, anie needs to draw its **own** scrollbar inside the output
pane.

### 2. Horizontal "scrolling" complaints are mostly markdown overflow

The output pane does not currently track any horizontal offset; the
paragraph scroll call hard-codes column offset `0`
(`crates/anie-tui/src/output.rs:391-393`).

But the user-visible example here is markdown tables, and the current
markdown renderer does let those overflow:

- `emit_table(...)` sizes columns to the widest cell
  (`crates/anie-tui/src/markdown/layout.rs:633-690`)
- `pad_cell(...)` explicitly returns the original content when it
  exceeds the column width, with a note that wrapping is out of scope
  (`crates/anie-tui/src/markdown/layout.rs:800-808`)
- code blocks do the same today: `build_code_body_line(...)` lets long
  lines push the right border onto the next visual row and explicitly
  punts proper horizontal scrolling to later
  (`crates/anie-tui/src/markdown/layout.rs:941-947`)

So the first fix for the "can't scroll side to side" complaint should be
to stop ordinary markdown tables from overflowing in the first place.

## What pi does here

pi's useful idea is on the **markdown** side, not on the scrollbar side.

### pi markdown rendering

pi's markdown component:

- renders to a width-aware line buffer
  (`packages/tui/src/components/markdown.ts:116-199`)
- wraps normal rendered lines with `wrapTextWithAnsi(...)`
  (`packages/tui/src/components/markdown.ts:151-158`)
- renders tables with width-aware column sizing and wrapped cell text
  (`packages/tui/src/components/markdown.ts:676-848`)
- falls back to raw wrapped markdown when the terminal is too narrow for
  a stable table
  (`packages/tui/src/components/markdown.ts:696-703`)

That means pi usually avoids transcript-wide horizontal overflow for
markdown by **fitting the content to the viewport width**.

### pi scrolling

pi's TUI renderer appears to be viewport-based rather than
scrollbar-widget-based:

- it builds a working line buffer
- computes `viewportStart`
- composites overlays into the visible viewport
  (`packages/tui/src/tui.ts:767-793`)

I did **not** find a draggable scrollbar widget in pi's TUI or
interactive-mode code. So the pi takeaway is:

1. borrow its width-aware markdown/table handling
2. design our own ratatui scrollbar + drag interaction for anie

## Design

### 1. Add an in-pane vertical scrollbar

Reserve a 1-column gutter on the right side of the transcript area and
render a scrollbar derived from:

- total rendered lines
- viewport height
- current scroll offset

This must be an anie-owned widget, not the terminal-emulator scrollbar.
The thumb should scale with the visible fraction of the transcript.

### 2. Add click + drag support for the scrollbar

Mouse support should expand from wheel-only to:

- click on thumb: begin drag
- drag thumb: update scroll proportionally
- click above/below thumb: page jump
- release: end drag

This likely needs:

- output-pane geometry tracking
- thumb hit-testing
- drag state in `App` or `OutputPane`

Keep wheel, PageUp / PageDown, Home / End, and auto-follow behavior
working exactly as they do now.

### 3. Fix markdown tables the way pi does: fit-to-width first

Do **not** start with transcript-wide horizontal panning as the primary
fix for tables. For the reported table case, the better solution is:

1. compute table widths against available terminal width
2. wrap cell contents within column widths
3. preserve alignment and box-drawing borders
4. fall back to raw wrapped markdown if the terminal is too narrow for a
   stable table

That matches pi's approach closely enough to borrow the idea without
copying the TypeScript literally.

### 4. Treat true horizontal panning as a narrow follow-up

After tables are width-aware, reevaluate whether horizontal panning is
still needed. If it is, scope it narrowly to content that genuinely
cannot wrap without losing meaning, such as:

- code fences
- very long raw / preformatted lines

That follow-up should introduce explicit horizontal-offset state only for
those cases, not for the entire transcript by default.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-tui/src/output.rs` | scrollbar rendering, gutter reservation, scroll geometry helpers |
| `crates/anie-tui/src/app.rs` | mouse click/drag handling and drag-state wiring |
| `crates/anie-tui/src/tests.rs` | scrollbar scaling, drag, and transcript overflow regressions |
| `crates/anie-tui/src/markdown/layout.rs` | width-aware table layout and fallback behavior |
| `docs/tui_responsiveness/` | only if the newer scroll/overflow plan needs cross-links back to Plan 04 |

## Phased PRs

### PR A — render a real transcript scrollbar

1. Reserve a right-side gutter in the output area.
2. Render a scrollbar track + thumb scaled from transcript length and
   viewport height.
3. Keep all existing keyboard/wheel scrolling behavior unchanged.
4. Add tests that the thumb size/position updates as transcript length
   changes.

### PR B — scrollbar mouse interaction

1. Add hit-testing for the scrollbar gutter.
2. Support click-to-page and thumb dragging.
3. Keep drag math proportional to `max_scroll()` and clamp correctly at
   top/bottom.
4. Add focused tests for:
   - dragging the thumb
   - clicking above the thumb
   - clicking below the thumb

### PR C — width-aware markdown tables

1. Replace widest-cell-only table sizing with viewport-constrained
   sizing.
2. Wrap cell contents within computed column widths.
3. Preserve header alignment, row separators, and existing box-drawing
   visuals.
4. Add a "too narrow" fallback to raw wrapped markdown instead of
   letting the table blow past the viewport.

### PR D — horizontal overflow follow-up (optional)

1. Reevaluate after PR C whether real horizontal panning is still needed.
2. If yes, scope it to non-wrappable blocks (likely code fences) rather
   than the whole transcript.
3. Choose the interaction only after confirming the remaining real
   problem:
   - Shift+wheel / modifier-based pan
   - keyboard pan
   - block-local horizontal viewport

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | `output_scrollbar_thumb_scales_with_total_content` | `anie-tui/src/tests.rs` |
| 2 | `output_scrollbar_thumb_moves_when_scrolled` | same |
| 3 | `scrollbar_drag_updates_scroll_offset` | same |
| 4 | `scrollbar_track_click_pages_up_or_down` | same |
| 5 | `wheel_and_keyboard_scroll_keep_scrollbar_in_sync` | same |
| 6 | `markdown_table_wraps_cells_to_fit_viewport_width` | markdown layout tests |
| 7 | `markdown_table_too_narrow_falls_back_to_wrapped_raw_markdown` | same |
| 8 | `table_alignment_is_preserved_after_wrapping` | same |
| 9 | `wide_markdown_table_no_longer_pushes_borders_offscreen` | TUI regression test |
| 10 | `horizontal_overflow_followup_tests` | only if PR D lands |

## Risks

- **Wrong scrollbar target:** the terminal emulator's side scrollbar is
  outside the app; trying to "fix" that instead of drawing an in-app
  scrollbar will not work.
- **Thumb math drift:** mapping between thumb position and
  `scroll_offset` can be off-by-one near the top/bottom if it uses total
  lines instead of `max_scroll()`.
- **Layout churn:** adding a gutter changes the render width available to
  every transcript block and therefore interacts with line caching.
- **Table regressions:** width-constrained tables must preserve existing
  alignment/border behavior and handle malformed rows.
- **Premature horizontal pan complexity:** adding transcript-wide
  horizontal scrolling before fixing table wrapping would solve the wrong
  problem and complicate the interaction model.

## Exit criteria

- [ ] The transcript draws an app-owned vertical scrollbar whose thumb
      scales with the visible fraction of the content.
- [ ] Mouse wheel, keyboard scroll, and scrollbar drag all keep the same
      scroll position in sync.
- [ ] Clicking the scrollbar track pages the transcript predictably.
- [ ] Wide markdown tables fit the viewport via wrapped cells or a
      graceful fallback.
- [ ] The reported table-overflow case no longer requires horizontal
      panning to remain readable.
- [ ] Any remaining need for horizontal scrolling is isolated to a
      follow-up scope with concrete examples.

## Deferred

- Replacing the terminal emulator's own scrollbar behavior.
- Transcript-wide horizontal panning by default.
- Horizontal scrolling for code fences unless a concrete post-table-wrap
  case still justifies it.
