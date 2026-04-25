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

**Full progression: baseline → all PRs (01–07):**

| Scenario | Baseline | After PR 06 | After PR 07 | Total Δ |
|----------|---------:|------------:|------------:|--------:|
| `scroll_static_600` (OutputPane only) | 316.68 µs | 228.06 µs | 245.36 µs | **-22.5%** |
| `stream_into_static_600` (OutputPane only) | 2.0047 ms | 763.42 µs | 774.48 µs | **-61.4%** |
| `keystroke_into_idle_app_600` | 500.23 µs | ~365 µs | ~380 µs | **-24%** |
| `keystroke_during_stream_600` | 496.44 µs | ~365 µs | ~380 µs | **-24%** |
| `keystroke_into_long_buffer` | 504.14 µs | ~366 µs | ~380 µs | **-25%** |
| `resize_during_stream` | 98.225 ms | 98.138 ms | **534.86 µs** | **-99.5%** |

**Per-PR contribution:**

- **PR 01** (input layout dedupe): -77 µs/keystroke (one of two
  doubled `layout_lines` walks eliminated).
- **PR 03** (cache-hit path cleanup): -73 µs scroll, -46 µs/keystroke
  — visible-slice borrow (the largest single win on the cache-hit
  path), animated-block count cache, status-bar `shorten_path` cache.
- **PR 04** (streaming hot path): -19 µs scroll (find_link_ranges
  dedupe), -38 µs streaming (bullet/box static-string borrow).
- **PR 05** (simplifications): code-health, no perf target.
- **PR 06** (per-line `Arc<Line>` sharing): **-1.1 ms streaming**
  (~60% of `stream_into_static_600`) — cache hits became refcount
  bumps instead of deep clones of each `Line` + `Span` + `Cow<str>`.
- **PR 07** (multi-width `LineCache`): **-97.5 ms resize**
  (~99.5% of `resize_during_stream`) — resize alternations between
  two widths now hit cache. Trades ~17 µs/+7.6% on `scroll_static_600`
  (Option pattern overhead vs. the previous direct field compare),
  but absolute scroll stays well under one frame at 60 fps.

Cumulative: **~120 µs/keystroke removed (-24%)**, **62% off
streaming render**, **99.5% off resize**. The user's
input-and-output sluggishness should feel materially better,
especially during streaming output and on terminal resize.
