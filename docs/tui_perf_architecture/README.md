# tui_perf_architecture — fixing perceived TUI sluggishness

The TUI "looks good but feels sluggish." This folder plans the
work to make it feel instantaneous without a library switch or
architectural rewrite.

## Conclusion up front

**Stay on ratatui.** Four parallel research passes — current-code
audit, pi architecture survey, Rust-TUI-landscape survey, and
user-perceivable-interaction audit — converged on the same
answer: ratatui is not the bottleneck. The bottleneck is a
combination of (a) unshipped fixes from the existing performance
review and (b) small architectural polish (synchronized output,
streaming coalescing, cache hardening).

Switching to iocraft, cursive, zi, a hand-rolled crossterm
renderer, or a Tauri/egui desktop port is **not** a perf move.
Those are product moves with different tradeoffs; none of them
close the gap being reported.

## Diagnosis — why it feels slow

The audit found the following still-present hot paths, in rough
order of impact:

1. **`wrap_spans` per-character allocations**
   (`crates/anie-tui/src/markdown/layout.rs:839–899`). Flattens
   spans into `Vec<(char, Style)>` — one entry per character.
   A 5000-char streaming response re-wraps at 30 fps, allocating
   millions of tuples/sec.
2. **Cache-hit deep clones**
   (`crates/anie-tui/src/output.rs:~453`). `LineCache` stores
   `Vec<Line<'static>>` un-wrapped; every cache hit clones every
   `Span`. 10 cached blocks × 50 lines × 30 fps is heavy for
   static content.
3. **Cache-write clones** (`output.rs:~490`). On miss, the
   computed lines are cloned into the cache and again into the
   output vec.
4. **Link-map rebuild every frame** (`output.rs:486–488`). The
   post-scan for clickable-URL ranges runs for all blocks on
   every draw, not just changed blocks.
5. **No synchronized output (BSU/ESU)** around `terminal.draw`.
   Modern GPU terminals (Ghostty, Kitty, Alacritty, WezTerm,
   Windows Terminal) can paint a partial frame, producing a
   tearing-flicker that reads as "laggy."
6. **Autocomplete not debounced** (`input.rs:145`). Every
   keystroke queries the suggestion provider synchronously.
7. **Spinner keeps redrawing during streaming even when no
   token has arrived** (`app.rs:213–220, 623–628`). The 100 ms
   idle tick forces a full `build_lines` just to advance a
   spinner frame.
8. **Resize invalidates every block cache at once**
   (`output.rs:174–178`). Rapid resize events trigger a full
   re-layout on the next frame for all 600 blocks.
9. **Streaming tokens re-wrap the whole response text per
   delta** (`output.rs:~465, 235`). Correctly bypasses the
   cache, but the wrap cost itself is what's expensive (see #1).

The existing `docs/tui_responsiveness/` and
`docs/code_review_performance_2026-04-21/04_tui_output_hot_path.md`
plans **already describe** the fixes for 1–3 (Arc-wrapping,
per-segment wrap allocation, helper cleanup). They haven't
landed yet. Items 4–9 are new to this folder.

## What Codex CLI does that we don't

OpenAI's Codex CLI (`codex-rs/tui/`) was raised as a
reference point. Verified findings from reading their
source:

- **Uses `pulldown-cmark` + `syntect` + `two-face` directly.**
  Same primitives as anie. Does **not** depend on the
  `tui-markdown` crate — contrary to a claim we received,
  that crate is a PoC with fewer features (no tables, no
  links) and isn't in Codex's dependency list.
- **`MarkdownStreamCollector` in `markdown_stream.rs`.**
  Buffers streamed text, commits only on `\n`. Per-newline
  (not per-delta) it re-parses the accumulated buffer and
  emits the newly-complete lines. Incomplete tail stays in
  the buffer.
- **`StreamController` + `StreamState` FIFO** for committed
  lines with arrival timestamps, plus a type-writer pacing
  tick. We want the first half (collector + FIFO), not the
  pacing tick.
- **Synchronized output** (DECSET 2026). Already our Plan 02.

The central win Codex gets that anie doesn't today: during a
stream, committed lines are rewrapped at most once per newline
arrival (not per delta), and the incomplete tail is cheap
plain-wrap not full markdown parse. This is Plan 08 in this
folder.

anie's current renderer is **not** "pseudo-markdown" — it has
2,216 LOC of pulldown-cmark + syntect code supporting
headings, code fences with syntax highlighting, tables, and
links. Codex's renderer and anie's renderer are comparable in
capability; what differs is the *streaming* architecture.

## What pi does that we don't

pi uses a custom TS TUI (`@mariozechner/pi-tui`) with:

- **Line-level differential rendering** — diffs old vs new line
  arrays and sends only changed ranges via ANSI.
- **Per-component markdown cache** keyed on `(text, width,
  theme)` with explicit invalidation.
- **16 ms (60 fps) frame budget** with `requestRender`-style
  debouncing.
- **Message-level (not token-level) streaming events** — the
  agent emits a complete message-state-so-far, so the UI always
  renders once per event.

anie's ratatui-based stack already does cell-level diffing at
the backend layer (better than line-level). What anie lacks is
the per-component caching and the width/theme-stable cache
keys. **Plan 03** in this folder ports that discipline without
rewriting to a pi-style renderer.

Anti-patterns we explicitly don't copy from pi:

- Line-level diffing *above* ratatui's cell-level diff — would
  duplicate work.
- 16 ms target — unnecessary for a coding agent; 30 fps is
  fine once the per-frame cost is reduced.
- Custom ANSI-emission layer — ratatui's Buffer is already
  optimal at our cell counts.

## Guiding principles

1. **Measure before optimizing further.** We've already
   shipped Plan 09 viewport slicing (10× speedup at 600 blocks)
   without flamegraph evidence of what's actually hot.
   Instrumentation + flamegraph comes first. This has already
   bitten the project once (Plan 09 helped the Paragraph
   render but the dominant cost was upstream in `build_lines`).
2. **Land the already-scoped work first.** Plan 04 from
   `docs/code_review_performance_2026-04-21/` is fully designed
   and unshipped. Before inventing new architecture, cash in
   the tickets that are already written.
3. **Small, observable PRs.** Every PR in every plan below
   should ship with a before/after number — either a
   microbenchmark, a stress-test delta, or at minimum a
   flamegraph comparison.
4. **No ratatui replacement.** The research is unanimous. A
   library swap is not a perf move and costs a month.
5. **No UI paradigm change.** Immediate-mode is correct for
   this workload. Don't adopt VDOM/React/Ink — pi's wins come
   from caching and diffing, not from a retained model.
6. **Single-plan focus per PR.** Mixing a Plan 02 change with a
   Plan 03 change makes the before/after numbers meaningless.

## Plans

| # | Plan | Focus | Size | Depends on |
|---|------|-------|------|------------|
| 01 | [Baseline measurement + flamegraph](01_baseline_measurement.md) | Profile the real hot path before any fix | Small | none |
| 02 | [Synchronized output (BSU/ESU)](02_synchronized_output.md) | DECSET 2026 wrap around every `terminal.draw` | Tiny | none |
| 03 | [Land Plan 04 hot-path fixes](03_land_plan_04.md) | Arc-wrap cache, per-segment wrap allocation, helper cleanup | Medium-Large | 01 |
| 04 | [Streaming coalescing + backpressure](04_streaming_coalescing.md) | Batch deltas into per-frame string append, bounded channel | Medium | 01 |
| 05 | [Cache hardening (resize + link-map)](05_cache_hardening.md) | Width/theme cache keys, per-block link-map, resize debouncing | Medium | 03 |
| 06 | [Quick wins (autocomplete, idle, mouse)](06_quick_wins.md) | Autocomplete debounce, spinner idle suppression, verify mouse filter | Small | 01 |
| 07 | [Fallback: pi-style line-diff layer](07_line_diff_layer_fallback.md) | Only consider if 02–06 don't close the gap | Large | 01–06 landed |
| 08 | [Codex-style streaming collector](08_streaming_collector.md) | Newline-gated commit + cached committed text, so per-delta cost is tail-only | Medium-Large | 03, 04 |

## Suggested landing order

1. **Plan 01** — baseline measurement. We don't ship more
   blind fixes after Plan 09 overshot its target.
2. **Plan 02** — synchronized output. Afternoon of work, zero
   risk, known payoff on GPU terminals.
3. **Plan 06** — quick wins. Small patches, independent of
   bigger work. Lands while 03 is in review.
4. **Plan 03** — the big one. Plan 04 fixes: Arc-wrapping,
   wrap rewrite, helper cleanup. Medium-large but already
   scoped upstream.
5. **Plan 04** — streaming coalescing. After 03, because the
   per-delta wrap cost is the problem 03 solves; with 03 in
   place we can see whether streaming batching still matters.
6. **Plan 05** — cache hardening. Edge cases (resize storm,
   link-map waste) that matter for long sessions but aren't
   the median-case hot path.
7. **Plan 08** — streaming collector. Big architectural
   change. Land only if Plans 03 + 04 combined don't bring
   streaming frame time to target. Plan 08 is the "halve it
   again" move when the easier wins are spent.
8. **Plan 07** — explicitly a fallback. Only open if
   post-01–06 **and 08** flamegraph still shows ratatui's
   own paint path as the bottleneck. Very unlikely.

## What's intentionally not in this plan set

- **Adopting the `tui-markdown` crate.** It's a maintained
  PoC with no table/link support, would be a regression
  against our 2,216-LOC custom renderer. Codex doesn't use
  it either.
- **Replacing our markdown renderer.** anie's markdown
  implementation in `crates/anie-tui/src/markdown/` is
  feature-complete (headings, fences with syntect, tables,
  links, lists). The sluggishness is not in markdown quality;
  it's in when and how often markdown rendering fires. Plans
  03 + 08 address that.
- **Switching off ratatui.** Research is unanimous; not a perf
  move. See "Alternatives to ratatui" in the research notes.
- **Custom crossterm renderer.** Would duplicate ratatui's
  Buffer diff. Negative ROI.
- **Desktop / web UI port.** Tauri/egui/iced/freya — all real
  options, but they're product choices, not perf fixes. Claude
  Code, Aider, Warp ADE all stay TUI-first for a reason
  (pipeability, SSH, tmux).
- **Separate OS thread for rendering.** ratatui is
  single-threaded by design; a render *task* decoupled via
  channel is what's wanted and already present
  (`app.rs:1431–1545`). An OS thread buys nothing.
- **Dirty-region partial redraws at app level.** ratatui
  already does cell-level diffs
  ([ratatui FAQ](https://ratatui.rs/faq/)). Duplicating that
  in app code is dead work.
- **60 fps target.** 30 fps is fine for a coding agent once
  per-frame cost is bounded. We have bigger problems than the
  cap.

## Milestone exit criteria

- [ ] Flamegraph from Plan 01 available at
      `docs/tui_perf_architecture/execution/flamegraph_baseline.*`
      (checked-in or referenced in execution README).
- [ ] Plans 01, 02, 03, 06 landed.
- [ ] At least one end-to-end scenario (600-block transcript,
      streaming response, rapid scrolling, resize storm)
      feels subjectively instantaneous on:
      - Ghostty / Kitty (GPU terminal, synchronized output)
      - iTerm2 / gnome-terminal (non-GPU baseline)
- [ ] `cargo bench -p anie-tui` (or equivalent stress test)
      shows no regression vs Plan 09 baseline, and a measurable
      improvement for at least one scenario.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] If Plan 07 is opened, it has an explicit justification
      citing flamegraph evidence that 01–06 were not enough.

## References

- `docs/tui_responsiveness/README.md` — prior perf plan
  (render scheduling, block cache design).
- `docs/code_review_performance_2026-04-21/04_tui_output_hot_path.md`
  — the existing-but-unshipped plan that Plan 03 here lands.
- `docs/code_review_performance_2026-04-21/10_tui_scrolling_and_markdown_overflow.md`
  — adjacent scrolling work; this plan set coordinates with it.
- `docs/markdown_perf/README.md` — prior markdown perf work
  that shipped (Plan 09 viewport slicing).
- Research sources for library alternatives, synchronized
  output, and pi's renderer are cited inline in
  `02_synchronized_output.md` and `07_line_diff_layer_fallback.md`.
