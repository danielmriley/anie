# tui_perf_architecture — execution tracker

Landing status per plan. Update as PRs merge. Keep numbers
from the Plan 01 benchmark next to each landed PR so the
perf trajectory is readable at a glance.

## Status

| Plan | PR | Status | Baseline delta | Notes |
|------|----|--------|----------------|-------|
| 01 | PR-A (perf-trace JSONL) | not started | — | |
| 01 | PR-B (criterion bench) | not started | — | |
| 01 | PR-C (flamegraph capture) | not started | — | |
| 02 | synchronized output | not started | — | |
| 03 | PR-A (Arc-wrap cache) | not started | — | |
| 03 | PR-B (wrap_spans rewrite) | not started | — | |
| 03 | PR-C (helper sweep) | not started | — | |
| 04 | PR-A (drain-batch deltas) | not started | — | |
| 04 | PR-B (bounded channel) | not started | — | |
| 05 | PR-A (BlockRender merge) | not started | — | |
| 05 | PR-B (per-block link cache) | not started | — | |
| 05 | PR-C (resize debounce) | not started | — | |
| 06 | PR-A (autocomplete debounce) | not started | — | |
| 06 | PR-B (stall-aware spinner) | not started | — | |
| 06 | PR-C (mouse motion trace) | not started | — | |
| 08 | PR-A (collector struct + tests) | not started | — | Codex-style streaming |
| 08 | PR-B (wire into OutputPane) | not started | — | |
| 08 | PR-C (finalize-flush + width change) | not started | — | |
| 07 | — | not open | — | fallback — trigger conditions unmet |

## Baseline numbers

To be populated by Plan 01 PR-B. Expected format:

```
scroll_static_600       p50=X.XXms  p95=X.XXms  p99=X.XXms
stream_into_static_600  p50=X.XXms  p95=X.XXms  p99=X.XXms
resize_during_stream    p50=X.XXms  p95=X.XXms  p99=X.XXms
```

## Flamegraph

Placeholder — to be added by Plan 01 PR-C.
`flamegraph_baseline.svg` (or equivalent) will live in this
directory.

## Subjective smoke list

For each landed plan, do a brief smoke test across:

- Ghostty (GPU, BSU-supporting) — streaming + scrolling a
  600-block transcript.
- gnome-terminal or iTerm2 (non-GPU baseline).
- tmux (proxy, quirks vary).

Record any regressions or improvements here, dated.
