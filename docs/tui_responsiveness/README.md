# TUI responsiveness plan

After a 53-minute agent run that produced 179 messages, the TUI
became slow to respond to input and eventually crashed. A surgical
fix in `df2d2c4` (drain agent events before redrawing, suppress
idle tick redraws) converted the worst O(n²) behavior into O(n),
but the per-frame cost still scales with the total transcript
size because `OutputPane::to_lines` re-wraps every block on every
render. A sufficiently long run still feels sluggish.

This folder plans the structural fix. Three PRs landing in order;
each is small and verifiable on its own.

## Guiding principles

1. **Follow pi's TUI model where it applies.** Pi's
   `packages/tui/src/tui.ts` proves out two patterns worth
   adopting: request-based render scheduling with a frame-rate
   cap, and per-component `(content, width) -> lines` caching.
   Both are documented inline in this folder.
2. **Use what ratatui already gives us.** Ratatui's
   `Terminal::draw()` double-buffers into a `Buffer` and writes
   only changed *terminal cells* to stdout. That already handles
   the line-level diffing pi does by hand. We don't need to copy
   it. The expensive piece is *building* the `Vec<Line>` each
   frame — that's a content-layer problem and that's what we
   cache.
3. **Don't reinvent.** No custom render scheduler, no custom
   differential pipeline, no parallel rendering thread. A frame-
   budget check plus per-block line caching is enough. Keep the
   structural change surgical so it's easy to review and revert.
4. **Measure before optimizing further.** Add light debug
   instrumentation so the next time someone says "the TUI feels
   slow" we have numbers instead of hunches.

## Execution — three PRs, in order

| PR | Scope | Cost | Impact |
|----|-------|------|--------|
| **PR 1** — [render scheduling + 30 fps cap](01_render_scheduling.md) | Rewrite the `run_tui` main loop to be request-based with a `FRAME_BUDGET = Duration::from_millis(33)` (30 fps) cap. | Small (~30 lines, one file). | Bounds the worst-case redraw *rate* regardless of transcript size or event burst rate. Prevents input starvation. |
| **PR 2** — [per-block line cache in OutputPane](02_output_pane_cache.md) | Each `RenderedBlock` caches its rendered `Vec<Line>` keyed by terminal width. Mutations invalidate. Spinner-bearing blocks skip cache. | Medium (~100-150 lines, one file). | Redraw cost scales with the number of *changing* blocks (usually 1 — the streaming assistant) instead of total transcript size. |
| **PR 3** — [debug instrumentation](03_debug_instrumentation.md) | Counter for full renders + optional env-gated log with render reasons. | Small (~30 lines). | Gives us measurable evidence next time; no user-visible behavior change. Lands last so we can confirm PR 1 + PR 2 actually fixed things. |

## Milestone exit criteria

- [ ] All three PRs merged in order.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] Manual: run an agent task that emits >500 assistant-text
      tokens at typical model speed on a 200-message-deep
      session. Typing in the input field stays responsive (no
      visible input lag).
- [ ] Manual: `ANIE_DEBUG_REDRAW=1` log shows redraw count
      remaining bounded (target: <= 30 / second under burst).

## What we're not doing

- **Parallel/off-thread rendering.** Ratatui runs
  single-threaded; pi runs single-threaded. The JS runtime's
  event loop doesn't prove parallelism is needed, and adding
  threads would require careful shared-state handling we don't
  currently need.
- **Line-level diffing of rendered output.** Ratatui's
  `Buffer` already does cell-level diffing when writing to the
  terminal. Re-implementing what pi does in TypeScript
  (`previousLines` array, first/last-changed detection) would
  duplicate infrastructure ratatui owns.
- **Content-shrink detection.** Ratatui handles this correctly
  within `Buffer`; we don't need pi's `maxLinesRendered` + clear
  logic.
- **Synchronized output escape sequences** (`\x1b[?2026h` ...).
  Ratatui's backend writer batches writes per frame; the
  synchronized-output protocol is a nice-to-have for avoiding
  tearing on terminals that support it, but not a correctness
  issue for us and not where the observed slowness comes from.

## Reference

- Pi's render loop: `pi/packages/tui/src/tui.ts` lines ~472-519
  (`requestRender`, `scheduleRender`) and ~888-1060 (`doRender`
  with differential compare). **Note: differential compare is
  what ratatui already does for us at the cell level — we don't
  port that.**
- Pi's per-component cache: `pi/packages/tui/src/components/markdown.ts`
  lines ~85-200 (cache keyed by `(text, width)`, invalidated on
  `setText`). **This is the pattern we adopt in PR 2.**
- Our current render path: `crates/anie-tui/src/app.rs::run_tui`
  and `crates/anie-tui/src/output.rs::OutputPane::{render,to_lines,block_lines}`.
- The partial fix in `df2d2c4` (drain + dirty) is the baseline
  PR 1 builds on.
