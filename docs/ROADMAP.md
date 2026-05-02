# Anie Roadmap

Single source of truth for what's shipped, what's
in flight, and what's next. Each row links to a plan
series or doc; series with their own status trackers
are linked at "see tracker."

Last consolidated: 2026-05-02.

## Active plan series

The major in-flight efforts. Each series owns its own
README + per-PR plan docs. The status column here is
high-level; the per-series tracker is authoritative.

| Series | Goal | Status | Tracker |
|---|---|---|---|
| **RLM + context virtualization** (`rlm_2026-04-29/`) | Recursive Language Models substrate: recurse tool, indexed external store, eviction policy, ledger injection, embedding-based reranker, background summarization | **All 6 phases (A-F) + Plan 08 embedding reranker landed on `dev_rlm`** | [`rlm_2026-04-29/execution/README.md`](rlm_2026-04-29/execution/README.md) |
| **Harness mitigations** (`harness_mitigations_2026-05-01/`) | Fix the loudest small-model failure modes from the 2026-05-01 smoke (hallucinated success on tool error, stuck loops, hallucinated improvements) | **PRs 1-3 + follow-up shipped on `dev_rlm`. PR 4 (relevance-based failed-result eviction) planned** | [`harness_mitigations_2026-05-01/README.md`](harness_mitigations_2026-05-01/README.md) |
| **Sub-agents + decompose + parallel recurse** (`rlm_subagents_2026-05-01/`) | Address the long-tail-reasoning gap (T2 stalled at 43 min): true sub-agents with full tools, decompose-and-recurse scaffolding, optional parallel recurse | **Planning. README + PR 1-2 docs written; PR 3-6 deferred pending user reaction** | [`rlm_subagents_2026-05-01/README.md`](rlm_subagents_2026-05-01/README.md) |
| **Skills system** (`skills_2026-05-02/`) | Anthropic-style skills: markdown files in `.anie/skills/` (and `.agents/skills/`) that the agent loads on demand. The discovery layer for the recurse/decompose capabilities | **Planning. README + PR 1-3 docs written; PR 4-5 deferred pending user reaction** | [`skills_2026-05-02/README.md`](skills_2026-05-02/README.md) |
| **REPL agent loop** (`repl_agent_loop/`) | Refactor `AgentLoop::run` into an explicit Read → Eval → Print → Loop runtime — the substrate that everything above ultimately rides on | **Planning + partial. See `repl_agent_loop_2026-04-27.md` for the original write-up** | [`repl_agent_loop/`](repl_agent_loop/) |
| **Provider expansion** (`add_providers/`) | Built-in support for OpenRouter (highest-priority), xAI, Groq, Cerebras, Mistral, Google Gemini, Azure OpenAI, OpenAI Responses API, Amazon Bedrock | **OpenRouter shipped (per memory). Others drafted as plans** | [`add_providers/README.md`](add_providers/README.md) |
| **Smoke protocol** (`smoke_protocol_2026-05-01.md`) | Canonical 11-turn DLL+weather scenario for validating context-virt and small-model harness changes | **Shipped; baseline captured 2026-05-01; re-run after each major series PR** | [`smoke_protocol_2026-05-01.md`](smoke_protocol_2026-05-01.md) |

## Cross-series coordination

The active series interact:

- **Skills** and **Sub-agents** are complementary —
  sub-agents give the *capability* to decompose and
  recurse; skills give the agent the *discovery
  handles* to use that capability under context
  pressure.
- **Harness mitigations** covers the
  reactive layer (handle failures gracefully); the
  other series cover the proactive layer (decompose
  hard problems, surface guidance).
- **Smoke protocol** is the validation layer all
  three feed into.

Cross-series PR ordering captured in
`skills_2026-05-02/README.md` ("Implementation order
across the two series").

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
- [x] **RLM substrate (Phases A-F + Plan 08)** — recurse tool,
      indexed external store, ceiling + FIFO eviction with
      pinned-tail, ledger injection, embedding-based reranker,
      background summarization. Ships under `--harness-mode=rlm`.
      See [`rlm_2026-04-29/execution/README.md`](rlm_2026-04-29/execution/README.md).
- [x] **Harness mitigations PR 1-3 + follow-up** — failed-tool-
      result wrap, observability-only failure-loop detector,
      re-test-after-edit rule (in rlm augment only). Caught and
      fixed the T10 wardrobe-refusal regression via the
      follow-up. See
      [`harness_mitigations_2026-05-01/README.md`](harness_mitigations_2026-05-01/README.md).
- [x] **11-turn smoke protocol baseline** — captured
      2026-05-01 against qwen3.5:9b. Re-run after the
      mitigations confirmed PR 1 working (model engages with
      failures), PR 2 correctly silent (args varied), PR 3
      regression caught and fixed. See
      [`smoke_protocol_2026-05-01.md`](smoke_protocol_2026-05-01.md).

## Next Up — Foundational Architecture

### 0. REPL-shaped agent loop — **plan series active**
Active plan series. The substrate that everything in
the RLM / sub-agents / skills work ultimately rides
on. See the "Active plan series" table at the top
for status; original write-up at
[`repl_agent_loop_2026-04-27.md`](repl_agent_loop_2026-04-27.md).

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

### 10. Skills system — **plan series active**
Now an active plan series under
[`skills_2026-05-02/`](skills_2026-05-02/). README +
PR 1-3 docs written. The system is structured as the
discovery layer for the recurse/decompose capabilities
the sub-agents series is building. See the "Active
plan series" table at the top of this document for
status.

### 11. `/settings` command
**What**: Interactive settings viewer/editor in the TUI.
**Why**: Currently all config changes require editing TOML files.
**Effort**: Medium-large — TUI overlay, config mutation, persistence.
**Details**: [docs/notes/commands_and_slash_menu.md](notes/commands_and_slash_menu.md)

### 12. Provider expansion — **plan series active**
OpenRouter shipped (per memory entries). Other
providers (xAI, Groq, Cerebras, Mistral, Gemini,
Azure OpenAI, OpenAI Responses API, Bedrock) drafted
as plans under
[`add_providers/`](add_providers/). See the "Active
plan series" table at the top for high-level status.

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
