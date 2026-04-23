# code_review_performance_2026-04-21 execution tracker

Status of the performance-cleanup plans derived from
`code_review_performance_2026-04-21.md`. Update inline as PRs land.

See also: [parallel_workstreams.md](parallel_workstreams.md) for the
conflict-minimizing multi-agent assignment layout.

## Plan status

| # | Plan | Status | Notes |
|---|------|--------|-------|
| 01 | Tool registry + schema validation | **landed** | 3/3 PRs shipped as Phase 1.2/1.3 of the refactor worklist |
| 02 | Agent turn ownership + event payloads | pending | clone-heavy run-loop cleanup |
| 03 | Session indexing + context construction | pending | single-index session simplification |
| 04 | TUI output hot path | **landed** | 6/6 PRs shipped; scroll_static_600: 3.16 ms → 296 µs (10.7×) |
| 05 | Picker search + fuzzy matching | pending | pi tokenized fuzzy ideas land here |
| 06 | Provider streaming + local models | pending | correctness-sensitive Anthropic work inside |
| 07 | Tool read/grep/bash/edit + truncation | pending | shared truncation helper likely starts here |
| 08 | Low-risk helper sweep | pending | land last |
| 09 | Tool output display modes | pending | UI-only `verbose` / `compact` transcript toggle for bash/read |
| 10 | TUI scrolling + markdown overflow | pending | app scrollbar + pi-style width-aware markdown table handling |

## PR breakdown

| Plan | PR | Scope | Status | Commit |
|------|----|-------|--------|--------|
| 01 | A | cached sorted definitions | **landed** | `baa0839` |
| 01 | B | precompiled validators | **landed** | `cd1cd06` |
| 01 | C | borrowed `definitions()` API (optional) | **landed** (as part of A) | `baa0839` |
| 02 | A | `Cow` sanitization fast path | pending | — |
| 02 | B | prompt replay ownership cleanup | pending | — |
| 02 | C | tool-result ownership cleanup | pending | — |
| 02 | D | `finish_with_assistant` cleanup | pending | — |
| 02 | E | centralized run-finalization | pending | — |
| 02 | F | optional `AgentEnd` payload change | pending | — |
| 03 | A | remove `id_set` | pending | — |
| 03 | B | `open_session` / `add_entries` clone cleanup | pending | — |
| 03 | C | borrowed branch walk | pending | — |
| 03 | D | `find_cut_point` trim | pending | — |
| 03 | E | lightweight `list_sessions` | pending | — |
| 03 | F | session-local helper sweep | pending | — |
| 04 | A | render scheduling cross-link / landing | **landed** (no-op — already shipped via `tui_responsiveness/`) | `3fb113d` |
| 04 | B | Arc-backed output cache storage | **landed** | `3fb113d` |
| 04 | C | cache read-side cleanup + invalidation audit | **landed** — 10.7× scroll speedup | `26f6e8a` |
| 04 | D | `wrap_plain_text` rewrite | **landed** | `458aea5` |
| 04 | E | `wrap_spans` rewrite | **landed** (both output.rs + layout.rs variants) | `646de92` |
| 04 | F | render helper cleanup | **landed** | `c058f51` |
| 05 | A | lowered-query scorer API | pending | — |
| 05 | B | tokenized model-picker filtering | pending | — |
| 05 | C | autocomplete lowercase caches | pending | — |
| 05 | D | text-field helpers | pending | — |
| 06 | A | Anthropic request-body ordering | pending | — |
| 06 | B | Anthropic empty-delta cleanup | pending | — |
| 06 | C | OpenAI empty-delta cleanup | pending | — |
| 06 | D | tagged reasoning splitter cleanup | pending | — |
| 06 | E | model-discovery cache ownership | pending | — |
| 06 | F | local probe normalization | pending | — |
| 06 | G | provider helper sweep | pending | — |
| 07 | A | shared truncation helper scaffold | pending | — |
| 07 | B | grep direct-write path | pending | — |
| 07 | C | bash tail rendering cleanup | pending | — |
| 07 | D | edit fuzzy normalization | pending | — |
| 07 | E | edit BOM / line-ending helper cleanup | pending | — |
| 07 | F | read-path cheap wins (output-body work) | pending | — |
| 07 | G | read helper cleanup | pending | — |
| 07 | H | streamed / size-gated read follow-up (optional) | pending | — |
| 08 | A | text assembly helper sweep | pending | — |
| 08 | B | token-estimation helper cleanup | pending | — |
| 08 | C | model-catalog cleanup | pending | — |
| 08 | D | remaining TUI/CLI helpers | pending | — |
| 09 | A | config + pane plumbing | pending | — |
| 09 | B | compact rendering for `bash` / `read` | pending | — |
| 09 | C | `/tool-output` runtime toggle | pending | — |
| 10 | A | render a real transcript scrollbar | pending | — |
| 10 | B | scrollbar mouse interaction | pending | — |
| 10 | C | width-aware markdown tables | pending | — |
| 10 | D | horizontal overflow follow-up (optional) | pending | — |

## Suggested landing order

1. 01A → 01B
2. 04A/04B
3. 09A → 09C
4. 10A → 10C
5. 05A → 05B
6. 02A → 02E
7. 03A → 03E
8. 06A → 06F
9. 07A → 07G
10. 08A → 08D
11. trailing follow-ups / optional work: 02F, 03F, 06G, 07H, 10D

## Per-PR gate

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Plus the plan-specific smoke checks in each numbered document.
