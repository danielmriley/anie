# anie — Documentation

Entry point for the anie-rs docs tree.

## Where things are

### If you want to ship something

- **[ROADMAP.md](ROADMAP.md)** — prioritized task list. What's next.
- **[notes/](notes/)** — per-topic brainstorms that feed the roadmap.
  Ideas, constraints, and design sketches; not committed plans.
- **[refactor_plans/](refactor_plans/)** — large not-yet-started
  refactors. Currently just the pi-shaped extension system (plan 10)
  plus the pi-mono comparison it's grounded in.
- **[code_review_2026-04-24/](code_review_2026-04-24/)** —
  implementation-ready hardening plans from the GPT-5.5 review,
  cross-checked against pi and Codex. Covers async auth, OpenAI image
  support, retry consistency, persistence visibility, resource caps,
  TUI event draining, and repo hygiene.
- **[add_providers/](add_providers/)** — plans for adding new LLM
  providers (OpenRouter, Gemini, xAI/Groq/Cerebras/Mistral, Azure,
  Bedrock, OpenAI Responses). Complements the `adding-providers`
  skill, which covers the mechanical how-to; these plans set
  priorities, model catalogs, and onboarding integration.

### If you want to understand how anie works today

- **[arch/anie-rs_architecture.md](arch/anie-rs_architecture.md)** —
  canonical architecture source of truth: crate ownership, runtime
  flow, provider/tool contracts, persistence formats, hot paths,
  known risks, and refactor guidance. Update this alongside
  architecture-significant code changes.
- **[arch/credential_resolution.md](arch/credential_resolution.md)** —
  how auth keys flow (CLI → keyring → JSON → env).
- **[arch/onboarding_flow.md](arch/onboarding_flow.md)** — first-run
  TUI flow.
- **[arch/pi_summary.md](arch/pi_summary.md)** /
  **[arch/codex_summary.md](arch/codex_summary.md)** /
  **[arch/codex_architecture_report.md](arch/codex_architecture_report.md)** —
  reference sketches of related agent projects. Useful as
  comparative material when making design choices.

### If you want to look up finished work

- **[completed/](completed/README.md)** — archive. Everything that
  shipped, organized so the original plans stay readable alongside
  the code they produced.

## Standing design proposals

Small, named proposals that aren't part of a phased plan:

- **[compat_system_plan.md](compat_system_plan.md)** — per-provider
  and per-model compatibility flags for OpenAI-compatible
  backends. Parked until real local-model problems drive the
  design.
- **[shell_escape_proposal.md](shell_escape_proposal.md)** — `!cmd`
  prefix in the TUI input pane. Not implemented.
- **[post_phase_telegram.md](post_phase_telegram.md)** — Telegram
  bot frontend via teloxide. Post-phase feature.

## Conventions

- Docs use **standard markdown with relative links**. Cross-references
  work in GitHub, any IDE markdown preview, and Obsidian. There's no
  wiki-style syntax or vault metadata.
- Plans live at the top level (or in `refactor_plans/`) while active.
  When the work ships, the plan moves under `completed/` with its
  history preserved by `git mv`.
- `completed/` is ordered by topic, not by date. Each subdir either
  mirrors its former top-level location (`next_steps/`,
  `onboarding_plans/`, `testing_phases/`, `refactor_plans/`) or holds
  standalone historical artifacts.
