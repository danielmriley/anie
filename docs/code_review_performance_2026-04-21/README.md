# code_review_performance_2026-04-21: implementation plan set

This folder turns `code_review_performance_2026-04-21.md` into a set
of small, implementation-ready plans. The review found a large number
of issues, but they cluster naturally into a handful of **PR-sized**
cleanup tracks:

- tool-registry / validation overhead
- agent-loop ownership and event cloning
- session indexing and context construction
- TUI output/render hot paths
- picker/autocomplete fuzzy search
- provider streaming / local model handling
- tool-output truncation and read/grep/bash/edit paths
- UI-only tool transcript display modes for bash/read
- TUI transcript scrolling and markdown overflow handling
- low-risk helper cleanups that are worth landing only after the hot
  paths are stable

## Guiding principles

1. **Small, verifiable PRs.** Each plan below is broken into small
   reviewable PRs (typically 1-6, depending on scope). No "one giant
   perf branch."
2. **Hot path first.** We land changes that fire every frame, every
   keystroke, every tool call, or every turn before sweeping up
   low-risk helpers.
3. **Preserve behavior first.** Performance fixes should not
   silently change provider semantics, transcript shape, or tool
   output. Where the review found correctness-sensitive code (for
   example Anthropic's thinking/temperature ordering), the plan
   keeps the behavior and removes the duplication around it.
4. **Reuse pi's structure only when it helps.** The pi comparison
   addendum surfaced three ideas worth borrowing:
   - tokenized fuzzy filtering + swapped-digit fallback
   - a single session ID index
   - shared truncation helpers across tools
   We adopt those where they fit; we do **not** blindly port pi.
5. **Respect existing plans.** `docs/tui_responsiveness/` already
   covers render scheduling and block caching. Plan 04 below folds
   the review findings into that work rather than duplicating it
   blindly.

## Relationship to existing docs

- `docs/tui_responsiveness/` already plans render scheduling and an
  `OutputPane` block cache. Plan 04 in this folder extends that
  work to cover the additional report findings (`wrap_plain_text`,
  `wrap_spans`, spinner-frame allocation, write-side cache clone,
  and transcript helper allocation patterns).
- The pi-comparison addendum at the end of
  `code_review_performance_2026-04-21.md` is the design input for
  the fuzzy-search, session-index, and truncation plans here.
- This folder is the operational plan set for the performance
  review. The review remains the source of findings; this folder is
  the source of implementation sequencing.

## Ordering and dependencies

| # | Plan | Findings | Size | Depends on |
|---|------|----------|------|------------|
| 01 | [Tool registry + schema validation](01_tool_registry_and_schema.md) | #1, #10 | Small | none |
| 02 | [Agent turn ownership + event payloads](02_agent_turn_ownership.md) | #2, #6, #7, #8, #9, #23 | Medium | none |
| 03 | [Session indexing + context construction](03_session_indexing_and_context.md) | #11, #12, #13, #24, #25, #26, #27, #38 | Medium | none |
| 04 | [TUI output hot path](04_tui_output_hot_path.md) | #3, #4, #5, #39, #47, #48, #52 | Medium-Large | uses `docs/tui_responsiveness/` |
| 05 | [Picker search + fuzzy matching](05_picker_search_and_fuzzy.md) | #43, #44, #45, #46, #51 | Medium | none |
| 06 | [Provider streaming + local models](06_provider_streaming_and_local_models.md) | #15, #17, #18, #49, #53, #54, #55 | Medium | none |
| 07 | [Tool read/grep/bash/edit + truncation](07_tool_read_find_grep_truncation.md) | #19, #20, #21, #31, #32, #33, #34, #36, #37, #56 | Medium-Large | none |
| 08 | [Low-risk helper sweep](08_low_risk_cleanup_sweep.md) | #14, #22, #28, #29, #30, #40, #41, #42, #50, #57 | Small-Medium | after 01-07 stabilize |
| 09 | [Tool output display modes](09_tool_output_display_modes.md) | feature follow-up (2026-04-21) | Small-Medium | after 04 to reduce `output.rs` churn |
| 10 | [TUI scrolling + markdown overflow](10_tui_scrolling_and_markdown_overflow.md) | feature follow-up (2026-04-21) | Medium | after 04 and 09 to reduce `output.rs` / markdown churn |

## Suggested landing order

The report's final priority ranking converts to the following
implementation order:

1. **Plan 01** — cheap, low-risk, unconditional win on every tool
   call.
2. **Plan 04 PR A/B** — output/render hot path already has a
   dedicated design doc; keep the TUI responsive first.
3. **Plan 09** — adds the new UI-only bash/read transcript mode
   after the output-pane work settles.
4. **Plan 10** — adds a real transcript scrollbar and fixes
   markdown table overflow using the useful parts of pi's
   renderer.
5. **Plan 05** — every keystroke path, structurally improved by the
   pi fuzzy-filter ideas.
6. **Plan 02** — removes deep-clone pressure from the main agent
   loop.
7. **Plan 03** — simplifies session internals and removes
   duplicated indices.
8. **Plan 06** — provider/local-model cleanup after the agent and
   session layers are simpler.
9. **Plan 07** — tool-path cleanup plus shared truncation helpers.
10. **Plan 08** — mop up the low-risk helpers only after the hot
   paths land and stabilize.

## Milestone exit criteria

- [ ] Plans 01-10 landed or explicitly deferred with rationale.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] Manual smoke across four scenarios:
  - long streaming TUI run (>200 transcript blocks)
  - rapid picker/autocomplete typing on a large model catalog
  - local OpenAI-compatible model discovery + streaming response
  - tool-heavy session using `read`, `grep`, `find`, `bash`, and
    `edit`
- [ ] No review item remains "important enough to act on" without a
      corresponding landed PR, explicit defer, or updated finding in
      the review document.

## What's intentionally not in this plan set

- A broad architectural rewrite of the agent/session/provider stack.
- Display-width / Unicode width correctness changes beyond the
  current review scope. The report explicitly called out that
  `wrap_plain_text` / `wrap_spans` fixes should preserve current
  `chars().count()` behavior unless a separate Unicode-width effort
  is approved.
- Replacing anie's richer local-model reasoning infrastructure with
  pi's much coarser `reasoning?: boolean` model; the pi comparison
  showed pi is **not** ahead there.

## Plans

| # | Plan | Focus |
|---|------|-------|
| 01 | [Tool registry + schema validation](01_tool_registry_and_schema.md) | Precompiled validators and cached tool definitions |
| 02 | [Agent turn ownership + event payloads](02_agent_turn_ownership.md) | Remove avoidable cloning on every run/turn |
| 03 | [Session indexing + context construction](03_session_indexing_and_context.md) | Single ID index, lighter branch/context builders |
| 04 | [TUI output hot path](04_tui_output_hot_path.md) | OutputPane rendering, wrapping, and transcript helper cleanup |
| 05 | [Picker search + fuzzy matching](05_picker_search_and_fuzzy.md) | Query normalization, tokenized filtering, text-field helpers |
| 06 | [Provider streaming + local models](06_provider_streaming_and_local_models.md) | Anthropic/OpenAI stream paths and local discovery |
| 07 | [Tool read/grep/bash/edit + truncation](07_tool_read_find_grep_truncation.md) | Shared truncation, read-path cleanup, grep/bash/edit fixes |
| 08 | [Low-risk helper sweep](08_low_risk_cleanup_sweep.md) | Remaining cheap wins after hot paths land |
| 09 | [Tool output display modes](09_tool_output_display_modes.md) | `verbose` vs `compact` transcript rendering for bash/read while keeping edit/write diffs visible |
| 10 | [TUI scrolling + markdown overflow](10_tui_scrolling_and_markdown_overflow.md) | App-drawn scrollbar, mouse drag, and width-aware markdown overflow handling |

## References

- `code_review_performance_2026-04-21.md` — source of findings.
- `docs/tui_responsiveness/README.md` — existing TUI hot-path plan.
- `docs/max_tokens_handling/README.md` — plan template.
- `docs/pi_adoption_plan/README.md` — multi-plan folder structure.
