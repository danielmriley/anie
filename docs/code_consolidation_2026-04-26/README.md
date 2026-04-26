# Code consolidation — 2026-04-26

Workspace-wide review of duplicated code, single-line helpers,
and abstractions that look speculative. Goal: reduce surface
area without behavior change. Reference points are pi
(`/home/daniel/Projects/agents/pi/`) and codex
(`/home/daniel/Projects/agents/codex/codex-rs/`) where shape
matters.

## Top-level findings (cross-crate)

Five parallel agents reviewed:
- CLI / controller (`anie-cli`)
- Provider crates (`anie-provider`, `anie-providers-builtin`)
- TUI rendering (`anie-tui` excluding markdown)
- Session / agent / auth / config (`anie-session`,
  `anie-agent`, `anie-auth`, `anie-config`)
- Markdown layout deep-dive (`anie-tui/src/markdown/`)

**Total identified LOC reducible without behavior change:
~800–1000.**

| Area | Severity | Est. LOC | Risk |
|------|---------:|---------:|-----:|
| Path helpers in anie-config | high | 40 | low |
| Atomic-write helper consolidation | medium | ~20 + safety win | low |
| Single-line wrappers across crates | medium | ~80 | low |
| Test fixture builder for controller_tests | medium | ~100 | low |
| Reasoning-family list dedupe (provider) | low | ~20 | low |
| Block render-helper consolidation in TUI | high | ~80 | medium |
| Overlay frame boilerplate | high | ~100 | medium |
| Markdown table layout simplification | high | 80–100 | medium |
| Markdown list-state machine | medium | 40–50 | medium |
| Tool-block dispatch logic | high | (consolidates) | medium |
| Spinner duplication (braille vs. breathing) | medium | TBD | medium |
| SSE streaming state machine consolidation | high | ~400 | **high** |
| OAuth provider deduplication | high | ~1,700 | **very high** |

## Files in this folder

- [`00_findings.md`](00_findings.md) — consolidated findings
  with `path:line` citations from all five agent reports.
- [`01_safe_wins.md`](01_safe_wins.md) — low-risk, high-
  certainty consolidations to land first. **Implementing
  these in the same branch as the plans** so the user can
  review code + plan together.
- [`02_tui_render_consolidation.md`](02_tui_render_consolidation.md)
  — block-helper merge, overlay frame extraction, spinner
  unification. Medium-risk; scope to one PR.
- [`03_markdown_simplification.md`](03_markdown_simplification.md)
  — adopt pi's table approach, simplify list state. Touches
  user-visible rendering; defer until UX validation of
  `tui_polish_2026-04-26` round.
- [`04_provider_streaming.md`](04_provider_streaming.md) —
  SSE state machine consolidation across OpenAI / Anthropic
  / Ollama. High risk; needs careful trait design.
- [`05_oauth_deduplication.md`](05_oauth_deduplication.md) —
  ~1,700 LOC across five OAuth providers. Highest impact /
  highest risk. Requires explicit user buy-in before starting.

## Suggested PR ordering

1. **PR 01 safe wins** — path helpers, atomic-write safety,
   single-line wrapper removals, deprecated `auth_file_path()`
   cleanup. Low risk, immediate code-health win. **Implemented
   on this branch.**
2. **PR 02 TUI render consolidation** — bullet-header helper,
   overlay frame extraction, spinner unification.
3. **PR 03 markdown simplification** — pending validation
   that nothing visually regresses from `tui_polish_2026-04-26`.
4. **PR 04 SSE state machine** — design pass first; implement
   only after the abstraction is reviewed.
5. **PR 05 OAuth dedup** — speculative; revisit only when
   adding the next OAuth provider provides a concrete pull
   on the abstraction.

## Principles

- **Don't simplify for simplification's sake.** Each removal
  needs a documented reason: deduplication, a clear
  boundary issue, or removing a footgun.
- **Behavior preservation first.** A consolidation that
  changes user-visible output is a feature change, not a
  cleanup. Belongs in a different round.
- **High-risk consolidations stay deferred.** SSE and OAuth
  are flagged for explicit user approval before starting.
  The cost of a regression in those layers is high.
