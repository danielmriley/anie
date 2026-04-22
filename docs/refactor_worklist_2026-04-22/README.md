# refactor_worklist_2026-04-22 — aggregated plans

This folder is an **aggregated snapshot** of every individual
plan referenced by the master worklist at
`docs/refactor_worklist_2026-04-22.md`. It exists so a
reviewer can read the whole refactor in one place without
jumping between two plan-set folders.

## Canonical homes

These files are **copies**. The canonical source for each
plan lives in its original folder and that's where PRs
update execution status. If a plan here contradicts its
canonical source, trust the canonical source and flag the
drift.

| File in this folder | Canonical source |
|--------------------|------------------|
| `tui_perf_01_baseline_measurement.md` | `docs/tui_perf_architecture/01_baseline_measurement.md` |
| `tui_perf_02_synchronized_output.md` | `docs/tui_perf_architecture/02_synchronized_output.md` |
| `tui_perf_04_streaming_coalescing.md` | `docs/tui_perf_architecture/04_streaming_coalescing.md` |
| `tui_perf_05_cache_hardening.md` | `docs/tui_perf_architecture/05_cache_hardening.md` |
| `tui_perf_06_quick_wins.md` | `docs/tui_perf_architecture/06_quick_wins.md` |
| `tui_perf_07_line_diff_layer_fallback.md` | `docs/tui_perf_architecture/07_line_diff_layer_fallback.md` |
| `tui_perf_08_streaming_collector.md` | `docs/tui_perf_architecture/08_streaming_collector.md` |
| `code_review_01_tool_registry_and_schema.md` | `docs/code_review_performance_2026-04-21/01_tool_registry_and_schema.md` |
| `code_review_02_agent_turn_ownership.md` | `docs/code_review_performance_2026-04-21/02_agent_turn_ownership.md` |
| `code_review_03_session_indexing_and_context.md` | `docs/code_review_performance_2026-04-21/03_session_indexing_and_context.md` |
| `code_review_04_tui_output_hot_path.md` | `docs/code_review_performance_2026-04-21/04_tui_output_hot_path.md` |
| `code_review_05_picker_search_and_fuzzy.md` | `docs/code_review_performance_2026-04-21/05_picker_search_and_fuzzy.md` |
| `code_review_06_provider_streaming_and_local_models.md` | `docs/code_review_performance_2026-04-21/06_provider_streaming_and_local_models.md` |
| `code_review_07_tool_read_find_grep_truncation.md` | `docs/code_review_performance_2026-04-21/07_tool_read_find_grep_truncation.md` |
| `code_review_08_low_risk_cleanup_sweep.md` | `docs/code_review_performance_2026-04-21/08_low_risk_cleanup_sweep.md` |
| `code_review_09_tool_output_display_modes.md` | `docs/code_review_performance_2026-04-21/09_tool_output_display_modes.md` |
| `code_review_10_tui_scrolling_and_markdown_overflow.md` | `docs/code_review_performance_2026-04-21/10_tui_scrolling_and_markdown_overflow.md` |

## Not included (by design)

- **`tui_perf_architecture/03_land_plan_04.md`** — deliberately
  omitted. That plan is an execution view of
  `code_review_04_tui_output_hot_path.md`, which *is* included.
  See the "Overlap resolution" section of
  `docs/refactor_worklist_2026-04-22.md` for the full call-out.
- **Execution trackers**
  (`tui_perf_architecture/execution/README.md`,
  `code_review_performance_2026-04-21/execution/README.md`).
  Those track live PR status — duplicating them here would
  guarantee drift. Follow the links in the master worklist
  instead.

## Reading order

Read `docs/refactor_worklist_2026-04-22.md` first for the
phased sequencing across all 17 plans. Then dive into
individual plans for rationale, design, test plan, and risks.

Fast index by phase (see the worklist for full PR
breakdown):

- **Phase 0 (measure):** `tui_perf_01_baseline_measurement.md`
- **Phase 1 (cheap parallel wins):**
  `tui_perf_02_synchronized_output.md`,
  `code_review_01_tool_registry_and_schema.md`,
  `tui_perf_06_quick_wins.md` (PR-A)
- **Phase 2 (TUI hot path):**
  `code_review_04_tui_output_hot_path.md`
- **Phase 3 (streaming architecture):**
  `tui_perf_04_streaming_coalescing.md`,
  `tui_perf_08_streaming_collector.md`
- **Phase 4 (cache hardening + polish):**
  `tui_perf_05_cache_hardening.md`,
  `tui_perf_06_quick_wins.md` (PR-B/C)
- **Phase 5 (TUI features):**
  `code_review_09_tool_output_display_modes.md`,
  `code_review_10_tui_scrolling_and_markdown_overflow.md`
- **Phase 6 (picker/fuzzy):**
  `code_review_05_picker_search_and_fuzzy.md`
- **Phase 7 (agent/session):**
  `code_review_02_agent_turn_ownership.md`,
  `code_review_03_session_indexing_and_context.md`
- **Phase 8 (provider streaming):**
  `code_review_06_provider_streaming_and_local_models.md`
- **Phase 9 (tool paths):**
  `code_review_07_tool_read_find_grep_truncation.md`
- **Phase 10 (low-risk sweep):**
  `code_review_08_low_risk_cleanup_sweep.md`
- **Optional/fallback:**
  `tui_perf_07_line_diff_layer_fallback.md` (+ the
  optional PRs listed in the worklist).

## Keeping this folder fresh

When a canonical plan changes, re-copy it here. A quick
one-liner:

```bash
# from repo root
rsync -a --itemize-changes \
  docs/tui_perf_architecture/0{1,2,4,5,6,7,8}_*.md \
  docs/code_review_performance_2026-04-21/0{1..9}_*.md \
  docs/code_review_performance_2026-04-21/10_*.md \
  docs/refactor_worklist_2026-04-22/ \
  --dry-run
```

(drop `--dry-run` when you actually want to sync). Rename
files to the `tui_perf_*` / `code_review_*` prefix
convention after copying — `rsync` only gets you 90% there.

A lighter alternative: delete this folder entirely and
regenerate it with the same `cp` script used to create it
whenever the master worklist changes. The folder has no
unique content; it's a pure packaging view.
