# TUI input responsiveness fix plan

## Rationale

The reported bug is not "the input widget is slow." The bug is that
**typing shares a paint with expensive transcript work**, so the
composer feels sluggish whenever the output pane is expensive to
rebuild.

Current-code evidence:

- `run_tui` already treats keystrokes as urgent and bypasses the
  33 ms frame budget for one paint (`crates/anie-tui/src/app.rs:1675-1724`).
  So the lag is **not** caused by waiting for the next frame slot.
- That urgent path still calls `app.render(f)`, and `App::render`
  paints the output pane before the input pane
  (`crates/anie-tui/src/app.rs:434-471`).
- `OutputPane::render` always calls `rebuild_flat_cache(...)`
  before painting (`crates/anie-tui/src/output.rs:458-510`).
- The flat-cache fast path is disabled whenever any animated block is
  present (`crates/anie-tui/src/output.rs:537-549`, `773-782`), so a
  streaming assistant or executing tool forces a rebuild on every
  active-frame paint.
- That rebuild still walks the whole transcript, clones cached block
  lines into the flat buffer, and recomputes the live block
  (`crates/anie-tui/src/output.rs:551-696`).
- The live assistant block renders the **full accumulated answer text
  as markdown every frame**, including while streaming
  (`crates/anie-tui/src/output.rs:1040-1075`), and markdown rendering
  is a full parse/layout pass (`crates/anie-tui/src/markdown/layout.rs:27-41`).

Fresh local benchmark evidence (2026-04-23):

```bash
cargo bench -p anie-tui --bench tui_render -- --warm-up-time 1 --measurement-time 3
```

| Scenario | Current frame time |
|----------|-------------------:|
| `scroll_static_600` | ~0.35 ms |
| `stream_into_static_600` | ~4.6 ms |
| `resize_during_stream` | ~133 ms |

That lines up with the code:

1. **Idle/static transcripts are cheap.**
2. **Streaming is materially more expensive because the live block
   bypasses the flat-cache fast path and reparses markdown.**
3. **Resize is the worst case by far because the final post-debounce
   paint still requires transcript-wide re-layout work.**

This plan is intentionally narrower than the older TUI perf docs. A
large fraction of the older backlog has already landed
(`docs/tui_perf_architecture/execution/README.md`,
`docs/tui_responsiveness/execution/README.md`). The remaining bug is
specifically **composer responsiveness under active transcript churn**.

## Design

### 1. Separate "composer must paint now" from "transcript must rebuild now"

Today `dirty` is effectively one bit: any visible change triggers a
full `app.render(...)`, and `OutputPane::render(...)` decides whether
to rebuild transcript state. That is too coarse for typing.

We should split render intent into at least:

- `composer_dirty`
- `transcript_dirty`
- `layout_dirty`

Then add an explicit urgent-input render mode:

- **Urgent composer paint**: the input pane, status bar, and spinner
  row paint immediately, but the output pane reuses the last materialized
  `flat_lines` / link-map snapshot instead of rebuilding them.
- **Full paint**: the existing path, used for agent deltas, scroll,
  resize-final, overlay changes, and any case where layout must change.

Key rule: if a keystroke arrives while a transcript update is pending,
the urgent paint may show the transcript **one frame stale**. That is
acceptable for this bug. The next non-urgent full paint catches the
transcript up. The user-visible priority is "the character I typed
appears immediately."

This is the direct fix for the current bug because it removes the
composer from the critical path of `build_flat_lines`.

### 2. Make the active streaming block cheap when a full paint is required

Even after urgent input paints stop rebuilding the transcript, full
paints during active streaming are still more expensive than they need
to be because the live assistant block reparses the entire accumulated
markdown body every frame.

Adopt the deferred Plan 08 idea, but narrowly:

- Maintain a streaming assistant buffer with:
  - committed markdown text
  - cached committed rendered lines keyed by `(width, theme)`
  - uncommitted tail text
  - plain-wrapped tail lines
- Commit on newline boundaries.
- Re-render committed markdown only when new committed text arrives or
  width/theme changes.
- Re-wrap the tail cheaply on each delta.

This keeps the current "no end-of-stream visual snap" goal while
removing the worst "full accumulated markdown every frame" behavior.

### 3. Reopen resize as its own input-responsiveness problem

The 50 ms debounce that already landed is correct, but it only removes
intermediate paints. The final resize paint is still very expensive.
For input responsiveness, that means typing during or immediately after
resize still competes with a transcript-wide rebuild.

If PR 1 + PR 2 do not bring resize-adjacent typing into the target
range, add a viewport-first resize path:

- On resize-final, synchronously rebuild only the blocks needed for the
  visible viewport plus a small overscan.
- Defer offscreen block reflow to later non-urgent frames.
- Keep scroll state conservative/correct even while the offscreen pass
  is incomplete.

This is intentionally gated because it is the most invasive change in
the plan. We should not pay that complexity cost unless the simpler
split-render approach still leaves resize-induced lag visible.

### 4. Measure the user-facing symptom directly

The existing render benchmark is useful, but it does not directly test
"I typed a character while streaming; how long until the character was
painted?"

Use the existing `ANIE_TRACE_TYPING` hook in `run_tui`
(`crates/anie-tui/src/app.rs:1689-1735`) as the canonical metric and
add a reproducible benchmark / scripted smoke around it.

The performance target for this bug should be phrased in user terms:

- **Typing during streaming:** keystroke-to-paint p95 feels instant and
  remains below one frame on typical hardware.
- **Typing around resize:** no visible multi-frame stall after resize
  quiesces.

## Files to touch

### PR 1 - urgent composer paint

- `crates/anie-tui/src/app.rs`
  - split dirty reasons / render intent
  - thread an urgent render mode into `App::render`
- `crates/anie-tui/src/output.rs`
  - allow urgent paints to reuse the last flat snapshot without calling
    `build_flat_lines`
- `crates/anie-tui/src/render_debug.rs`
  - optional counters to distinguish urgent-input paints from full paints

### PR 2 - streaming live-block cheap path

- `crates/anie-tui/src/output.rs`
  - streaming assistant buffer / committed+tail collector
  - active-block rendering changes
- `crates/anie-tui/src/markdown/mod.rs`
  - only if a small helper is useful for cached committed markdown
- `crates/anie-tui/benches/tui_render.rs`
  - add or refine a streaming-heavy benchmark that grows the live block

### PR 3 - resize hardening (gated)

- `crates/anie-tui/src/output.rs`
  - viewport-first rebuild support
- `crates/anie-tui/src/app.rs`
  - resize-final scheduling / follow-up work queue
- tests in `crates/anie-tui/src/tests.rs` or nearby in-module tests

### Measurement / docs

- `crates/anie-tui/benches/tui_render.rs`
- `docs/tui_perf_architecture/execution/README.md`
- `docs/tui_perf_architecture/execution/baseline_numbers.md`

## Phased PRs

### PR 1 - urgent composer paints skip transcript rebuild

**Goal:** when the user types, the composer paints immediately without
paying `build_flat_lines`.

**Change:**

1. Split render dirtiness into composer/transcript/layout causes.
2. Introduce an urgent-input render mode.
3. In that mode, `OutputPane` paints from the existing `flat_lines`
   snapshot and does not rebuild transcript state.
4. Keep `transcript_dirty = true` so the next non-urgent frame catches
   the transcript up.

**Why first:** this is the most direct fix for the reported bug and does
not require changing the markdown renderer.

**Exit criteria:**

- A keystroke-only paint does not call `build_flat_lines`.
- `ANIE_TRACE_TYPING=1` shows keystroke paints no longer blocked by
  transcript rebuild work.
- No correctness regressions in scrolling, mouse hit-testing, or overlay
  rendering.

### PR 2 - make active streaming blocks incremental

**Goal:** reduce the cost of the full paints that still happen while a
stream is active.

**Change:**

1. Replace "render full accumulated assistant markdown every frame" with
   committed markdown + plain tail.
2. Cache committed rendered markdown by width/theme.
3. Re-wrap only the tail on each streaming delta.
4. Finalize the block into the normal cached finalized form when the
   stream ends.

**Why second:** PR 1 fixes typing feel immediately; PR 2 lowers the
baseline cost of all active-stream paints and reduces the chance that
agent output starves full paints generally.

**Exit criteria:**

- `stream_into_static_600` improves materially from the current
  ~4.6 ms baseline.
- Long streamed answers do not show the current "cost grows with the
  full accumulated block every frame" profile.
- Final rendered output remains byte-identical to rendering the same
  markdown as a finalized block.

### PR 3 - resize hardening if PR 1 + PR 2 are insufficient

**Goal:** keep resize-final paints from causing visible post-resize
stutter or typing lag.

**Change:**

1. Rebuild only the visible viewport synchronously after resize
   quiesces.
2. Defer offscreen block reflow to later non-urgent frames.
3. Preserve correct scroll semantics and link hit-testing while the
   deferred work completes.

**Why gated:** it is more complex than the first two PRs and should land
only if the user-visible problem remains after the simpler fixes.

**Exit criteria:**

- `resize_during_stream` drops materially from the current ~133 ms
  baseline.
- Typing immediately after resize no longer shows a visible multi-frame
  stall.

## Test plan

1. **Urgent-paint regression test**
   - drive a keystroke into an app with a live streaming block
   - assert the urgent paint path does not rebuild transcript state

2. **Typing-latency smoke**
   - run with `ANIE_TRACE_TYPING=1`
   - type continuously while a long answer streams
   - verify p95 keystroke-to-paint stays below one visible frame on the
     dev machine

3. **Streaming growth benchmark**
   - extend `tui_render` so the streaming scenario grows a large live
     block instead of only appending tiny constant chunks
   - compare before/after on the same machine

4. **Final-render parity**
   - stream markdown in many deltas
   - finalize
   - compare the rendered output with the same markdown rendered as a
     finalized block from the start

5. **Resize + typing smoke**
   - stream output
   - resize the terminal or tmux pane
   - type immediately after release
   - verify no visible input stall

## Risks

- **One-frame stale transcript during typing.**
  This is intentional in PR 1. The bug being fixed is input lag, so the
  correct tradeoff is to prefer immediate composer feedback over
  perfectly up-to-the-microsecond transcript freshness.

- **Streaming collector visual differences.**
  Partial markdown is tricky. The collector must preserve the current
  "no end-of-stream reformat snap" goal and must not regress code-fence,
  list, or table rendering on finalization.

- **Resize hardening complexity.**
  Viewport-first resize is the first part of this plan that meaningfully
  increases renderer state complexity. Keep it gated unless the simpler
  PRs fail to close the bug.

## Exit criteria

- Typing while a response is streaming feels immediate.
- Urgent keypress paints are no longer dominated by output-pane rebuilds.
- The live assistant block no longer reparses the full accumulated
  markdown body every active frame.
- Resize is either demonstrably acceptable after PR 1 + PR 2 or has a
  dedicated follow-up PR that fixes the remaining lag.
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.

## Deferred / explicitly not doing

- Replacing ratatui.
- Adding a separate render thread.
- Reintroducing autocomplete debounce on the main composer path.
- Rewriting the whole markdown renderer.
- Building a general app-level dirty-region renderer above ratatui.

The fix for this bug is narrower: **make typing stop paying for output
work, then make the remaining output work cheaper.**
