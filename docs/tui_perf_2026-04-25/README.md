# TUI perf review — 2026-04-25

User report (`feat/ollama-memory-safety` branch): "I still feel
sluggishness in the input and a little in the output."

The previous TUI perf rounds (`docs/completed/tui_perf_architecture/`,
`docs/completed/tui_responsiveness/`, the input-responsiveness
plan archived alongside) shipped the structural pieces — urgent
keystroke paint, drain-batch coalescing, Arc-wrapped block cache,
progressive-streaming markdown commit boundaries, etc. The
criterion benches improved as designed. Yet the user still feels
lag.

This round investigates *why*: the benches measure a slice of
the pipeline that doesn't exercise the user's actual workflow,
and several genuine inefficiencies remain on the per-keystroke
and per-frame paths.

## Files in this folder

- [`00_report.md`](00_report.md) — investigation findings, every
  item with a `path:line` citation. Read this first.
- [`01_input_pane_layout_dedupe.md`](01_input_pane_layout_dedupe.md)
  — fix the double `layout_lines()` call per keystroke. Smallest,
  highest-confidence input win.
- [`02_keystroke_latency_bench.md`](02_keystroke_latency_bench.md)
  — add a benchmark that measures the user's actual pain
  (key→paint), so future regressions don't hide.
- [`03_cache_hit_path_cleanup.md`](03_cache_hit_path_cleanup.md)
  — flat-cache visible-slice clone, `has_animated_blocks` walk,
  status-bar `shorten_path`. Per-frame work that fires even on
  Idle paints.
- [`04_streaming_hot_path.md`](04_streaming_hot_path.md) — link
  scan throttle on streaming blocks, pre-computed bullet/box
  headers, theme in streaming cache key.
- [`05_simplifications.md`](05_simplifications.md) — separable
  refactors that aren't perf fixes but reduce the surface area
  the perf paths have to defend (dispatch match collapse, scroll
  arm merge, dead `_is_streaming` parameter).

## Suggested PR ordering

1. **PR 02 first** (the bench). Land before the fixes so we can
   actually measure them. Without this, every PR below is graded
   on subjective feel.
2. **PR 01** (input layout dedupe). Smallest change, removes a
   doubled O(n) walk. Likely the single biggest contributor to
   the residual input feel.
3. **PR 03** (cache-hit path). Per-frame work that affects every
   paint mode. Idle paints regain headroom; urgent paints stop
   paying for cached state they're meant to be reusing.
4. **PR 04** (streaming hot path). Targets the "output feels a
   little sluggish" half of the report; only fires while a
   response is streaming.
5. **PR 05** (simplifications). Optional cleanup. Land if there's
   time, skip if not.

## Principles

- **Evidence-first.** Every claim has a `path:line` citation.
  When the bench number disagrees with the user's experience,
  the user's experience wins — but we still measure both.
- **One PR, one logical change.** Each plan lists a single PR
  with its own test list and exit criteria.
- **Don't over-engineer.** The TUI is already perf-architectured.
  This round is targeted fixes, not another sweep.
- **Benchmarks must reflect the user's pain.** The existing
  `tui_render` bench measures `OutputPane::render` against a
  TestBackend with no `App`, no input pane, no keystroke
  pipeline. PR 02 closes that gap before we declare anything
  fixed.

## Baseline (2026-04-25)

Captured against current `feat/ollama-memory-safety` HEAD
(after PR 02 lands):

**OutputPane-only (existing benches):**

| Scenario | Mean | Δ vs. 2026-04-22 |
|----------|-----:|-----------------:|
| `scroll_static_600` | 316.68 µs | +1.3% |
| `stream_into_static_600` | 2.0047 ms | -1.4% |
| `resize_during_stream` | 98.225 ms | +1.7% |

The OutputPane numbers haven't moved materially. The user's
complaint is real even though the bench numbers are stable,
which is itself the finding behind PR 02.

**App + InputPane (new in PR 02):**

| Scenario | Mean |
|----------|-----:|
| `keystroke_into_idle_app_600` | 500.23 µs |
| `keystroke_during_stream_600` | 496.44 µs |
| `keystroke_into_long_buffer` | 504.14 µs |

These first three numbers are the gate values for PR 01, 03, 04.
A keystroke in the App pipeline costs ~184 µs more than a bare
`OutputPane::render` on the same transcript — that delta is the
input-pane + status-bar + render-mode overhead that PRs 01 and
03 target.

The long-buffer scenario (F-1's target) is barely worse than
idle — only ~4 µs delta — which says the absolute cost of
the doubled `layout_lines` walk is small relative to the per-
frame floor (visible-slice clone, status-bar formatting, etc.).
PR 01 will help input scaling but the dominant residual will
come from PR 03's per-frame floor work.
