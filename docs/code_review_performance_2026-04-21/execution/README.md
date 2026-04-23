# code_review_performance_2026-04-21 execution tracker

Status of the performance-cleanup plans derived from
`code_review_performance_2026-04-21.md`. Update inline as PRs land.

See also: [parallel_workstreams.md](parallel_workstreams.md) for the
conflict-minimizing multi-agent assignment layout.

## Plan status

| # | Plan | Status | Notes |
|---|------|--------|-------|
| 01 | Tool registry + schema validation | **landed** | 3/3 PRs shipped as Phase 1.2/1.3 of the refactor worklist |
| 02 | Agent turn ownership + event payloads | **partial** | PR-A (Cow sanitization) landed; B-F deferred as small residual cleanups. |
| 03 | Session indexing + context construction | **partial** | PR-A (remove id_set) + PR-D (trim CutPoint.kept) landed; B/C/E/F deferred. |
| 04 | TUI output hot path | **landed** | 6/6 PRs shipped; scroll_static_600: 3.16 ms → 296 µs (10.7×) |
| 05 | Picker search + fuzzy matching | **landed** | 4/4 PRs shipped as Phase 6.1-6.4 |
| 06 | Provider streaming + local models | **landed** | 7/7 PRs shipped as Phase 8.1-8.7 |
| 07 | Tool read/grep/bash/edit + truncation | pending | shared truncation helper likely starts here |
| 08 | Low-risk helper sweep | pending | land last |
| 09 | Tool output display modes | **landed** | 3/3 PRs shipped as Phase 5.1-5.3 |
| 10 | TUI scrolling + markdown overflow | **landed** | 3/4 PRs shipped (A/B/C); PR-D horizontal pan deferred per plan gate |

## PR breakdown

| Plan | PR | Scope | Status | Commit |
|------|----|-------|--------|--------|
| 01 | A | cached sorted definitions | **landed** | `baa0839` |
| 01 | B | precompiled validators | **landed** | `cd1cd06` |
| 01 | C | borrowed `definitions()` API (optional) | **landed** (as part of A) | `baa0839` |
| 02 | A | `Cow` sanitization fast path | **landed** | `d953196` |
| 02 | B | prompt replay ownership cleanup | **deferred** | — | Medium-risk loop reordering; no user-visible impact beyond Cow win. Safe to revisit per plan. |
| 02 | C | tool-result ownership cleanup | **deferred** | — | Same as PR-B. |
| 02 | D | `finish_with_assistant` cleanup | **deferred** | — | Function-local; can land independently. |
| 02 | E | centralized run-finalization | **deferred** | — | Touches exit paths — land after B/C. |
| 02 | F | optional `AgentEnd` payload change | **deferred** (by plan design) | — | Plan says "only if still needed after E". |
| 03 | A | remove `id_set` | **landed** | `06c86e3` |
| 03 | B | `open_session` / `add_entries` clone cleanup | **deferred** | — | Small residual clones; follow-up. |
| 03 | C | borrowed branch walk | **deferred** | — | Small residual allocations in branch walk. |
| 03 | D | `find_cut_point` trim | **landed** | `c97bf48` |
| 03 | E | lightweight `list_sessions` | **deferred** | — | Only fires on `/session list` — not a keystroke path. |
| 03 | F | session-local helper sweep | **deferred** | — | Cheap wins; fold into a follow-up. |
| 04 | A | render scheduling cross-link / landing | **landed** (no-op — already shipped via `tui_responsiveness/`) | `3fb113d` |
| 04 | B | Arc-backed output cache storage | **landed** | `3fb113d` |
| 04 | C | cache read-side cleanup + invalidation audit | **landed** — 10.7× scroll speedup | `26f6e8a` |
| 04 | D | `wrap_plain_text` rewrite | **landed** | `458aea5` |
| 04 | E | `wrap_spans` rewrite | **landed** (both output.rs + layout.rs variants) | `646de92` |
| 04 | F | render helper cleanup | **landed** | `c058f51` |
| 05 | A | lowered-query scorer API | **landed** | `189c6ed` |
| 05 | B | tokenized model-picker filtering | **landed** | `189c6ed` |
| 05 | C | autocomplete lowercase caches | **landed** | `c22266c` |
| 05 | D | text-field helpers | **landed** (cursor_x cleaned; render_value deferred per plan) | `5a09649` |
| 06 | A | Anthropic request-body ordering | **landed** | `4c0b657` |
| 06 | B | Anthropic empty-delta cleanup | **landed** | `5bbcea6` |
| 06 | C | OpenAI empty-delta cleanup | **landed** | `5bbcea6` |
| 06 | D | tagged reasoning splitter cleanup | **landed** | `1837952` |
| 06 | E | model-discovery cache ownership | **landed** | `97ee40e` |
| 06 | F | local probe normalization | **landed** | `0b52586` |
| 06 | G | provider helper sweep | **landed** | `0b52586` |
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
| 09 | A | config + pane plumbing | **landed** | `597d702` |
| 09 | B | compact rendering for `bash` / `read` | **landed** | `e33724d` |
| 09 | C | `/tool-output` runtime toggle | **landed** | `15d48c8` |
| 10 | A | render a real transcript scrollbar | **landed** | `58f1203` |
| 10 | B | scrollbar mouse interaction | **landed** | `ec14168` |
| 10 | C | width-aware markdown tables | **landed** | `976f0f5` |
| 10 | D | horizontal overflow follow-up (optional) | **deferred** | — | Gate: PR-C resolved the reported table-overflow case. Re-evaluate only if a concrete non-wrappable block (e.g., code fence) still needs pan. |

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
