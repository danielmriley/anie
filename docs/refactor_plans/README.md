# anie — Active refactor plans

This folder holds refactor plans that have **not** landed yet. The
multi-phase pi-parity refactors that shipped (plans 00–08 plus the
`fixes/` follow-ups) have moved to
[`../completed/refactor_plans/`](../completed/refactor_plans/).

## Plans

| # | Title | Scope | Status |
|---|---|---|---|
| 10 | [Extension system (pi-shaped port)](./10_extension_system_pi_port.md) | New `anie-extensions` from scratch: JSON-RPC subprocesses, 35+ event types, tool/command/shortcut/flag/provider/renderer registration | Not started. Multi-phase (7 phases), ~6 weeks of focused work. |
| 13 | [Controller responsiveness + reliable action delivery](./13_controller_responsiveness_and_action_delivery.md) | Non-blocking retry backoff (Phase A) + unbounded TUI→controller action channel (Phase B). Unblocks merge-to-main. | Not started. 2 phases. Target: pre-merge. |
| 14 | [Persistence safety: atomic writes + auth-store discipline](./14_persistence_safety.md) | `atomic_write` utility + migration of seven call sites (Phase A); fail-loud on corrupted auth.json with quarantine backup (Phase B). Unblocks merge-to-main. | Not started. 2 phases. Target: pre-merge. |

Plan 09 is intentionally reserved for a future "tools parity with pi"
plan (`find`, `grep`, `ls`). Not written yet; deferred because tool
additions warrant individual careful review.

Plans 11 and 12 (graceful slash-command dispatch + inline
autocomplete popup) shipped in full; see
[`../completed/refactor_plans/`](../completed/refactor_plans/).

Plans 13 and 14 are the pre-merge blockers called out in the
2026-04-19 code-review cluster and the project-status summary.
They're grouped because #13 and #14 have tightly-related motivations
(both tightened by the followup review) but touch disjoint code —
they can be developed and reviewed in parallel.

## Background

[`pi_mono_comparison.md`](./pi_mono_comparison.md) — detailed mapping
of anie's architecture against pi-mono. Informed the revisions to
plans 02, 03, 04, 07 (see their doc bodies for specifics) and is the
design ground for plan 10.

## How this folder is organized

- **This README** — index of active plans.
- **Active plan files** — numbered, each self-contained (motivation,
  design principles, phases ≤5 files each, test plan per phase,
  exit criteria).
- **`pi_mono_comparison.md`** — reference material used by multiple
  plans.

When an active plan ships in full, `git mv` it to
[`../completed/refactor_plans/`](../completed/refactor_plans/). See
that folder's `README.md` for the landed history.

## What landed already

Summary — full detail is under
[`../completed/refactor_plans/`](../completed/refactor_plans/):

- **00** CI enforcement — clippy + fmt gated.
- **01** `openai.rs` module split + streaming tests.
- **02** TUI overlay trait + shared widgets + overlays directory.
- **03** Controller decomposition — `ModelCatalog`, `SessionHandle`,
  `ConfigState`, `SystemPromptCache`, registry, `RetryPolicy`.
- **04** Shared HTTP client + unified discovery (narrowed scope —
  tool-call assembler deferred).
- **05** Provider error taxonomy — typed `ProviderError` variants,
  no more string-matching.
- **06** Session write locking via `fd-lock`.
- **07** `anie-extensions` stub removal (precondition for plan 10).
- **08** Small hygiene items (`.anie/` path helper, HTTP-client
  fallback, `send_event` helper with warn-once latch, cached
  `ToolRegistry`, non-cloning context API).
- **11** Graceful slash-command dispatch — `UserCommandError`
  classification, `ArgumentSpec` on `SlashCommandInfo`, TUI-side
  pre-dispatch validation. A bad `/thinking` arg (or any bad
  user input) now surfaces as a system message instead of
  killing the controller.
- **12** Inline slash-command autocomplete popup — reusable
  `SelectList` widget, `AutocompleteProvider` trait, builtin
  command completer, popup overlay above/below the input,
  `ui.slash_command_popup_enabled` toggle, extension-ready
  contract.
- **Fix plans** under `../completed/refactor_plans/fixes/` —
  follow-ups that closed out partial exit criteria on plans 01,
  02, 03 (phases 3–5), 06, 07, 08.
- **Review + report** — `implementation_review_2026-04-18.md`
  captured the gap assessment; `implementation_report_fixes.md`
  is the wrap-up.
