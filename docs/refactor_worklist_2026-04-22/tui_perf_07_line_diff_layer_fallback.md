# Plan 07 — pi-style line-diff layer (fallback only)

## Rationale

pi's custom TS TUI diffs old vs new rendered line arrays and
sends only the changed ranges as ANSI updates
(`packages/tui/src/tui.ts:984–1115`). The research phase
surfaced this as a candidate pattern for anie.

**We explicitly don't open this plan unless 01–06 haven't
closed the gap.** The reason: ratatui *already does cell-level
diffing* at the backend layer
([ratatui FAQ](https://ratatui.rs/faq/)). Adding a
line-level diff on top duplicates work and can only pay off
if ratatui's diff itself is the bottleneck — which would be a
surprising result at our cell counts.

This plan exists as a **documented fallback** so future-us
knows what to consider if flamegraph evidence after Plans
01–06 still shows ratatui's paint path dominating. It's not
currently in the landing order.

## Trigger conditions

Open this plan only if **all** of:

- Plans 01–06 are landed.
- Plan 01's flamegraph shows >30% of frame time in
  ratatui's `Frame::render_widget` or `backend::flush` for
  the `scroll_static_600` scenario.
- Subjective reports of sluggishness persist.

Any other signal — markdown hot, wrapping hot, channel
backpressure, resize stutter — points back to the earlier
plans, not here.

## Rough design (sketch only)

- Render the frame to a `Vec<Line<'static>>` as today.
- Keep the previous frame's `Vec<Line>` in state.
- Diff line-by-line; compute `first_changed` and
  `last_changed` indices.
- For the changed range, render only those lines via
  ratatui's `Paragraph` into a sub-region of the buffer.
- Keep the rest of the previous buffer intact.

This is a **layer above ratatui**. The trick is that ratatui's
`Terminal::draw` normally produces a full `Buffer`; bypassing
that to produce a partial buffer means drawing directly to
`terminal.backend_mut()` with crossterm commands. Risky — you
lose the framework's guarantees.

Alternative: keep ratatui's full `terminal.draw`, but provide
a custom `Widget` for `OutputPane` that internally tracks its
previous `Vec<Line>`, compares, and writes only changed lines
to the shared `Buffer`. Cells outside the changed range
remain whatever they were from the previous frame's buffer,
and ratatui's cell-diff handles the actual terminal writes.

This second option is what **rooibos-rs** and similar libraries
do; it's ratatui-compatible. It's the path to prototype first
if this plan opens.

## Files that would change (not committing)

- A new `crates/anie-tui/src/widgets/diff_widget.rs` with the
  custom widget.
- `crates/anie-tui/src/output.rs` to render through the new
  widget instead of `Paragraph`.
- Tests for diff correctness: identical inputs produce
  byte-identical output to the non-diff path.

## Risks (if plan opens)

- **Correctness is hard.** Partial redraws are a famous
  source of "ghost characters" bugs. Full-width character
  handling, SGR reset sequences, wide-char continuation —
  all traps. ratatui has already solved this at the cell
  level; re-solving at the line level gives you nothing
  unless the cell-diff itself is slow.
- **Interaction with synchronized output (Plan 02).** Must
  remain correct inside BSU/ESU.
- **Debugging partial-render bugs in the wild.** Users can't
  send you a flamegraph; they can send a screenshot of a
  corrupted frame. Hard to chase.

## What success looks like

A flamegraph after this plan lands shows:

- ratatui's `Frame::render_widget` / `Buffer::merge` /
  `crossterm::QueueableCommand` self-time drops.
- No new "diff widget" functions in top-20.
- Subjective smoothness on `scroll_static_600` improves.

If any of those don't hold: roll back. The baseline without
this plan is correct and acceptable; the prize is only real
if the measured bottleneck is exactly ratatui's own paint.

## Exit criteria (if plan opens)

- [ ] Trigger conditions above met and documented in
      `execution/`.
- [ ] Custom widget lands; existing visual tests pass
      byte-identical output to the `Paragraph` baseline.
- [ ] Flamegraph delta: ratatui-internal self-time drops.
- [ ] Three-terminal smoke (Ghostty, gnome-terminal, tmux)
      shows no rendering artifacts under streaming + resize.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.

## Deferred (structurally)

- Rewriting the whole `OutputPane` as a pi-style
  line-emitter (custom ANSI writer, no ratatui). Would bypass
  ratatui entirely. Not appropriate for anie — we'd lose the
  widget ecosystem (overlays, input pane, picker) for a
  speculative gain.

## Why this is plan seven, not plan three

The research phase's summary:

> ratatui is not the bottleneck. The bottleneck is a
> combination of unshipped fixes from the existing
> performance review and small architectural polish.

We land the unshipped work (03), the small polish (02, 04,
05, 06), and we measure (01) first. If all of that still
leaves us with a sluggish feel, *then* we open this plan with
evidence. Opening it earlier is putting an architectural
change ahead of the boring-but-correct fixes — which is how
you get Plan 09's outcome (real win, wrong target) again.
