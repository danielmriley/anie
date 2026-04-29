# Anie Roadmap

Unified, prioritized task list. Items are ordered by impact-to-effort
ratio — smallest impactful changes first. Check off items as they ship.

## Completed

- [x] Fix reasoning-only completion bug (reasoning_fix_plan.md phases 1–3)
- [x] Thinking block display regression tests (7 tests added)
- [x] Dynamic model discovery and picker menus
- [x] Onboarding flow
- [x] Context file hot-reload (mtime-based per-turn refresh)
- [x] `/copy` command (clipboard copy of last assistant response)
- [x] `/new` command (start fresh session without restart)
- [x] `/reload` command (hot-reload config and context files)
- [x] Graceful slash-command dispatch (plan 11): `/thinking bogus`
      no longer locks the controller; pre-dispatch validation on
      `SlashCommandInfo::validate`
- [x] Inline slash-command autocomplete popup (plan 12): `/` opens
      a filterable palette; arg-value completions for
      `Enumerated`/`Subcommands` specs; toggle via
      `ui.slash_command_popup_enabled`
- [x] Controller responsiveness (plan 13A): Ctrl+C / Abort / Quit
      drain promptly during transient-retry backoff via
      non-blocking `PendingRetry::Armed` state in the main
      `select!` loop
- [x] Reliable UI-action delivery (plan 13B): unbounded
      `UiAction` channel; user submit/quit/abort can no longer
      be silently dropped under controller backpressure
- [x] Persistence safety (plan 14): `anie_config::atomic_write`
      helper (temp-file + fsync + rename) for all user-facing
      writes; corrupt `auth.json` is quarantined to a sibling
      rather than silently overwritten
- [x] Long-running generation no longer restarts (commit `f85fdb8`):
      removed the 300-second whole-request timeout from the shared
      reqwest client so local-model streams can run to completion
- [x] TUI state RAII-guarded (commit `4030c64`): terminal is
      restored via `Drop`, so panics or early returns no longer
      leave the shell emitting SGR mouse-tracking escape
      sequences on clicks/scrolls
- [x] API-integrity suite (plans 00–06 of the api_integrity
      track, now in [`completed/api_integrity_plans/`](completed/api_integrity_plans/)):
      Anthropic thinking-signature replay, redacted-thinking
      support, round-trip audit, `ReplayCapabilities` on `Model`,
      cross-provider invariants, error taxonomy, session schema
      migration, multi-turn integration tests

## Next Up — Foundational Architecture

### 0. REPL-shaped agent loop — **top priority**
**What**: Refactor `AgentLoop::run` into an explicit
Read → Eval → Print → Loop runtime while preserving current behavior
first.
**Why**: Creates stable step boundaries for error recovery, proactive
compaction, context augmentation, queued user steering, verifier loops,
recursive task decomposition, and stronger local-small-model behavior.
This benefits frontier models too.
**Effort**: Large, staged refactor — first land behavior-characterization
tests, then extract run state, then introduce internal intents /
observations / decisions.
**Details**: [docs/repl_agent_loop_2026-04-27.md](repl_agent_loop_2026-04-27.md)

## Next Up — Small, High-Impact

### 1. Automatic context compaction
**What**: Trigger compaction automatically when approaching the context limit.
**Why**: Prevents context overflow errors. Currently compaction exists but
must be triggered manually or by overflow recovery.
**Effort**: Medium — threshold detection, automatic trigger, TUI indicator.
**Details**: [docs/notes/local_model_support.md](notes/local_model_support.md)

### 6. Local model context length detection
**What**: Query Ollama/vLLM for actual context window size instead of
defaulting to 32K.
**Why**: Incorrect context length leads to either wasted capacity or
overflow errors.
**Effort**: Medium — API queries, config override, caching.
**Details**: [docs/notes/local_model_support.md](notes/local_model_support.md)

### 7. Slash command autocomplete menu — **shipped**
Landed via plans 11 and 12. Typing `/` opens a filterable popup
that reads from the `SlashCommandInfo` catalog; argument values
complete for `Enumerated` (e.g. `/thinking`) and `Subcommands`
(e.g. `/session`) specs. Disable with
`ui.slash_command_popup_enabled = false` in `~/.anie/config.toml`.
File-path `@` completion remains a follow-up.

### 8. Session management commands (`/resume`, `/session`, `/name`)
**What**: Browse past sessions, show session info, set display names.
**Why**: Session management currently requires CLI flags or filesystem
knowledge.
**Effort**: Medium — session listing UI, metadata display.
**Details**: [docs/notes/commands_and_slash_menu.md](notes/commands_and_slash_menu.md)

## Longer-Term — Features

### 9. TUI layout improvements
**What**: Input area bars, region separation, user message styling.
**Why**: Visual clarity — the three TUI regions blur together.
**Effort**: Medium-large — layout restructuring, theme tokens.
**Details**: [docs/notes/tui_layout_and_visual_design.md](notes/tui_layout_and_visual_design.md)

### 10. Skills system
**What**: Agent Skills standard support — load SKILL.md files, register
as `/skill:name` commands.
**Why**: Enables repeatable, project-specific agent behaviors.
**Effort**: Medium — file loading, frontmatter parsing, command registration.
**Details**: [docs/notes/skills_system.md](notes/skills_system.md)

### 11. `/settings` command
**What**: Interactive settings viewer/editor in the TUI.
**Why**: Currently all config changes require editing TOML files.
**Effort**: Medium-large — TUI overlay, config mutation, persistence.
**Details**: [docs/notes/commands_and_slash_menu.md](notes/commands_and_slash_menu.md)

### 12. Provider expansion — **plans drafted**
**What**: Built-in support for OpenRouter (top priority), xAI,
Groq, Cerebras, Mistral, Google Gemini, Azure OpenAI, OpenAI
Responses API, and Amazon Bedrock.
**Why**: Broader model access without manual config.
**Effort**: Ranges from S (OpenRouter) to L (Bedrock). Most are
OpenAI-compat and add as a preset entry + catalog rows.
**Details**: [docs/add_providers/README.md](add_providers/README.md)
lists priorities. Per-provider plans live beside it.
**Skill**: `.claude/skills/adding-providers/SKILL.md` covers the
mechanical how-to that every plan cross-references.

## Long-Term — Architecture

### 13. Internet search tool
**What**: Self-hosted search via SearXNG + page content extraction.
**Details**: [docs/notes/internet_search_tool.md](notes/internet_search_tool.md)

### 14. Memory system
**What**: Persistent graph-based memory across sessions.
**Details**: [docs/notes/memory_system.md](notes/memory_system.md)

### 15. Daemon and messaging integrations
**What**: Background daemon with Telegram/Discord frontends.
**Details**: [docs/notes/daemon_and_messaging.md](notes/daemon_and_messaging.md)

### 16. Benchmarks and evaluation
**What**: Internal benchmark suite, TerminalBench investigation.
**Details**: [docs/notes/benchmarks_and_evaluation.md](notes/benchmarks_and_evaluation.md)

## Refactors

Refactors 00–08 plus the fix-plan follow-ups all landed. See
[`completed/refactor_plans/`](completed/refactor_plans/) for the
history. One active refactor remains:

| # | Refactor | When to do it |
|---|----------|---------------|
| 10 | [Extension system (pi-shaped port)](refactor_plans/10_extension_system_pi_port.md) | Multi-week; blocked on OAuth for phase 7, otherwise ready to start |

## Design documents (parked / proposals)

- [Compat system plan](compat_system_plan.md) — per-model backend flags.
  Parked until real local model problems drive the design.
- [Shell escape proposal](shell_escape_proposal.md) — `!cmd` prefix in
  the TUI input pane.
- [Post-phase Telegram integration](post_phase_telegram.md) — Telegram
  bot frontend via teloxide.

The thinking-only completion bug fix plan (phases 1–3, all shipped)
is archived at
[`completed/reasoning_fix_plan.md`](completed/reasoning_fix_plan.md).
