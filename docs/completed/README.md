# Completed work — archive

This directory holds plans, reviews, and proposals whose work has
shipped. Files are kept (not deleted) because they record the
reasoning behind what's now in the code — the commit log says
*what*, these docs say *why*.

If you're looking for what to work on next, see
[`../ROADMAP.md`](../ROADMAP.md). If you're looking for how the code
works today, see [`../arch/`](../arch/).

## Organization

Subdirectories mirror the planning area they came from. Standalone
files are listed alphabetically below, each with a one-line
description and (where the doc carries one) a reference date.

### Subdirectories

| Path | What's in it |
|---|---|
| [`refactor_plans/`](refactor_plans/) | Plans 00–08 (CI, openai split, TUI overlays, controller decomposition, provider unification, error taxonomy, session locking, extensions stub removal, hygiene), **11 + 12** (graceful slash-command dispatch + inline autocomplete popup), **13 + 14** (controller responsiveness + persistence safety — pre-merge hardening), the `fixes/` follow-ups, the review, and the implementation report |
| [`api_integrity_plans/`](api_integrity_plans/) | The replay-fidelity and capability-routing work that made the provider layer safe across multi-turn replay. Plans 00 (principles), 01a–e (Anthropic thinking signatures), 02 (redacted thinking), 03/a/b/c/d (round-trip audit, unsupported blocks, `ReplayCapabilities` on `Model`, cross-provider invariants), 04 (error taxonomy), 05 (session schema migration), 06 (multi-turn integration tests). All shipped; prerequisite reading for the `adding-providers` skill. |
| [`next_steps/`](next_steps/) | Four quick wins: context file hot-reload, `/copy`, `/new`, `/reload`. All shipped. |
| [`onboarding_plans/`](onboarding_plans/) | Dynamic model menus and inline picker (Phases 1–6). Also contains an earlier `completed/` sub-archive of the v0.1.0 keyring + onboarding phases. |
| [`testing_phases/`](testing_phases/) | Integration test suite build-out (Phases 0–4). Produced the `anie-integration-tests` crate. |
| [`phase_detail_plans/`](phase_detail_plans/) | v1.0 milestone phase plans (foundation, providers, TUI, sessions, extensions, hardening). |
| [`phased_plan_v1-0-1/`](phased_plan_v1-0-1/) | The v1.0.1 reasoning + local-model compatibility steps. |
| [`prompts/`](prompts/) | Implementation prompts used to drive multi-phase builds. |

### Standalone files

| File | Description | Date |
|---|---|---|
| [`anie-rs_build_doc.md`](anie-rs_build_doc.md) | Original build document. Superseded by `arch/anie-rs_architecture.md`. | — |
| [`IMPLEMENTATION_ORDER.md`](IMPLEMENTATION_ORDER.md) | Execution sequence for the initial v1.0 build. | — |
| [`IMPLEMENTATION_ORDER_V_1_0_1.md`](IMPLEMENTATION_ORDER_V_1_0_1.md) | Execution sequence for v1.0.1. | — |
| [`integration_testing_plan.md`](integration_testing_plan.md) | Proposal that produced `testing_phases/`. | 2026-04-15 |
| [`local_model_thinking_plan.md`](local_model_thinking_plan.md) | Local-model thinking/reasoning plan. | — |
| [`notes.md`](notes.md) | Early planning issue tracker from the build-doc era. | — |
| [`onboarding-and-keyring.md`](onboarding-and-keyring.md) | v0.1.0 credential store + TUI onboarding design. | — |
| [`project_review_2026-04-17.md`](project_review_2026-04-17.md) | Project review that spawned `refactor_plans/00–08`. | 2026-04-17 |
| [`reasoning_fix_plan.md`](reasoning_fix_plan.md) | Thinking-only completion bug fix (Phases 1–3). | — |
| [`runtime_state_integration_plan.md`](runtime_state_integration_plan.md) | Runtime state persistence integration. | — |
| [`status_report_2026-04-15.md`](status_report_2026-04-15.md) | Early status snapshot; superseded by the 2026-04-17 review. | 2026-04-15 |
| [`thinking_block_display_bug.md`](thinking_block_display_bug.md) | Bug note fixed by `reasoning_fix_plan.md`. | — |
| [`v1-0-1_review.md`](v1-0-1_review.md) | Post-v1.0.1 review. | — |
| [`v1_0_milestone_checklist.md`](v1_0_milestone_checklist.md) | v1.0 release sign-off checklist. | — |

## A note on dates

Where a filename or frontmatter carries a date, it's the date the
doc was written — not the date the work finished. The code is the
authoritative "finished" marker; use `git log` on the relevant
crate if you need to know exactly when something landed.
