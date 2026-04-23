# tui_perf_architecture — execution tracker

Landing status per plan. Update as PRs merge. Keep numbers
from the Plan 01 benchmark next to each landed PR so the
perf trajectory is readable at a glance.

## Status

| Plan | PR | Status | Baseline delta | Notes |
|------|----|--------|----------------|-------|
| 01 | PR-A (perf-trace JSONL) | **landed** | commit `306e4e3` | JSONL spans for 6 hot functions, writes to `~/.anie/logs/perf.log.<pid>` |
| 01 | PR-B (criterion bench) | **landed** | this commit | 3 scenarios; numbers in `baseline_numbers.md` |
| 01 | PR-C (top-5 + flamegraph) | **landed** | commit `cd06e21` | Top-5 self-time via PerfSpan aggregation; flamegraph recipe deferred (sysctl `perf_event_paranoid=4`). Priority flip: `markdown_render` is the real bottleneck, `wrap_spans` much less so. |
| 02 | synchronized output | **landed** | commit `f524233` | DECSET 2026 wrap around the main `terminal.draw`; unit tests use CrosstermBackend<Arc<Mutex<Vec<u8>>>> adapter. |
| 03 | PR-A (Arc-wrap cache) | not started | — | |
| 03 | PR-B (wrap_spans rewrite) | not started | — | |
| 03 | PR-A (Arc-wrap cache) | **landed** (as part of Plan 04 PR-B) | commit `3fb113d` | |
| 03 | PR-B (wrap_spans rewrite) | **landed** (as part of Plan 04 PR-E) | commit `646de92` | |
| 03 | PR-C (helper sweep) | **landed** (as part of Plan 04 PR-F) | commit `c058f51` | |
| 04 | PR-A (drain-batch deltas) | **landed** | commit `38eda8e` | Coalesces consecutive TextDelta/ThinkingDelta runs into one append per run. |
| 04 | PR-B (bounded channel) | **landed** (pre-existing) | — | Agent→UI channel was already `mpsc::channel(256)` with awaiting sends in `anie-cli/src/{interactive_mode,print_mode}.rs`. Producer-side `send_event` in `anie-agent/src/agent_loop.rs` uses `.send().await` so backpressure naturally applies. No code change needed. |
| 08 | PR-A/B/C (streaming collector) | **deferred** | — | Gate from `docs/refactor_worklist_2026-04-22.md`: 3.1+3.2 + Phase 2 reduced `stream_into_static_600` from 3.35 ms → 2.19 ms, well under the 33 ms budget. Collector is not needed for current sluggishness; revisit if long-stream performance regresses. |
| 05 | PR-A (BlockRender merge) | **subsumed** (Phase 2 PR-C) | — | Arc-backed `LineCache` already holds lines + links side-by-side. Separate `BlockRender` struct is unnecessary. |
| 05 | PR-B (per-block link cache) | **subsumed** (pre-existing behavior) | — | `find_link_ranges` already runs only on cache-miss and is stored in the Arc-backed `LineCache.links`, reused on hit. |
| 05 | PR-C (resize debounce) | **landed** | commit `4f0329d` | 50 ms debounce in the main `run_tui` loop; drag-in-progress skips intermediate paints. |
| 06 | PR-A (autocomplete debounce) | **landed** | commit `162d414` | 80 ms debounce, eager-on-first, popup-consumer flush, test-only `flush_pending_autocomplete_for_test` helper. |
| 06 | PR-B (stall-aware spinner) | **landed** | commit `4f0329d` | 500 ms stream-stall window; `needs_tick_redraw` suppresses spinner redraws when a stream is stuck. |
| 06 | PR-C (mouse motion trace) | **deferred** | — | Investigation-only; no real-user report of mouse starvation. Can open if data surfaces via the existing `ANIE_PERF_TRACE` output. |
| 08 | PR-A (collector struct + tests) | not started | — | Codex-style streaming |
| 08 | PR-B (wire into OutputPane) | not started | — | |
| 08 | PR-C (finalize-flush + width change) | not started | — | |
| 07 | — | not open | — | fallback — trigger conditions unmet |

## Baseline numbers

Captured 2026-04-22. See
[`baseline_numbers.md`](./baseline_numbers.md) for full
methodology + criterion CIs + re-run instructions.

| Scenario | Mean frame time |
|----------|----------------:|
| `scroll_static_600` | 3.20 ms |
| `stream_into_static_600` | 3.40 ms |
| `resize_during_stream` | 104.00 ms |

Key takeaway: cache-hit render is fine; resize-storm is the
primary sluggishness. Phase 4 cache hardening is the real
payoff target.

## Flamegraph

Capture deferred — host `kernel.perf_event_paranoid=4`
blocks user-mode perf recording. Recipe for running with
sudo-tweaked sysctl, plus alternative tools (samply,
pprof-rs), documented in
[`baseline_numbers.md`](./baseline_numbers.md). JSONL spans
from PR-A are a partial substitute.

## Subjective smoke list

For each landed plan, do a brief smoke test across:

- Ghostty (GPU, BSU-supporting) — streaming + scrolling a
  600-block transcript.
- gnome-terminal or iTerm2 (non-GPU baseline).
- tmux (proxy, quirks vary).

Record any regressions or improvements here, dated.
