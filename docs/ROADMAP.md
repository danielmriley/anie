# Anie Roadmap

Unified, prioritized task list. Items are ordered by impact-to-effort
ratio — smallest impactful changes first. Check off items as they ship.

## Completed

- [x] Working-tree checkpoint / rewind (docs/session_ux/, Workstream B):
      `/rewind` restores **both** the conversation and the working tree to
      a prior user turn; `/checkpoint [name]` records a labeled anchor. A
      content-addressed shadow store (`WorkspaceCheckpointStore` in
      `anie-session/src/checkpoint.rs`) keeps sha256-deduplicated blobs +
      a manifest in a `<session_id>.checkpoints/` sidecar; capture is at
      the user-turn boundary over the same write/edit set compaction
      tracks. Restore refuses on a typed `WorkingTreeDrifted` (no silent
      clobber) and reuses the existing append-only `SessionManager::fork`
      for the conversation half. Ships at session schema v4 (no bump), no
      new third-party crate.
- [x] Fork / rewind branch summaries (docs/session_ux/, Workstream C):
      both `/fork` and `/rewind` now leave a `SessionEntry::BranchSummary`
      (schema bump 4→5) preserving the forked-from / discarded branch's
      file operations, so abandoned work isn't lost from the log. `/fork`
      records it on the child (always current schema); `/rewind` records
      the discarded descendants on the rewound branch, gated on the file
      already being v5 so legacy v4 sessions stay readable by older
      binaries. Metadata only — context reconstruction ignores it.
      Closes SESSION-4.
- [x] Interactive session picker (docs/session_ux/, Workstream A):
      `/session` with no argument opens a search-first `BottomPane`
      picker (fuzzy filter over id + first message, wraparound
      navigation, current-session marker) instead of dumping state. A
      new flat `AgentEvent::SessionList` / `SessionSummary` protocol pair
      carries the list from the controller (which maps `SessionInfo` at
      one boundary) to the TUI; `Enter` switches, `Esc` cancels. The
      `/session list` and `/session <id>` text paths are retained.
      Closes SESSION-1.
- [x] Process sandbox for tool execution (docs/tool_sandbox/): opt-in,
      Linux-only `[tools.sandbox]` that confines the `bash` tool's spawned
      child via Landlock (writes restricted to roots) + seccomp (network
      off by default), installed child-only via `pre_exec` in the new
      `anie-sandbox` crate. Fail-closed by default; typed
      `ToolError::SandboxSetup`. Off by default = unchanged behavior. The
      approval layer (#1) is the recommended companion; escalation seam
      left open. macOS/Windows and in-process file-tool confinement
      deferred.
- [x] Eval harness + metrics export (docs/eval_harness/): `--metrics-out`
      writes a per-run `RunMetrics` JSON (tokens/latency/tool success/cost/
      compaction); the new leaf `anie-evals` crate runs TOML scenarios via
      the real `anie` binary under each `--harness-mode`, scores them with
      deterministic automated checks, and emits a JSON + Markdown
      comparison report. First cut — LLM-as-judge and multi-turn deferred.
- [x] Cost / token-budget enforcement (docs/cost_budget/): derive
      `Usage.cost` from catalog pricing, a session cost meter surfaced in
      `/state` and the TUI status bar, and optional `[budget]` run/session
      cost+token ceilings enforced as a typed `BeforeModelResponse::
      StopRun` (clean halt, partial work saved — not a ProviderError, not
      a panic). Opt-in; byte-identical when unset.
- [x] `apply_patch` tool (docs/apply_patch_tool/): a Codex-style
      `*** Begin Patch` envelope (Add/Update/Delete) applying multi-hunk,
      multi-file changes through the shared `text_match` engine and a new
      `FileMutationQueue::with_locks` primitive, with validate-all-then-
      write-all atomicity, `dry_run` preview, and whitespace-fuzzy match
      reporting on both `edit` and `apply_patch` (EDIT-1/2/3/5). Rename
      and a cross-file journal deferred.
- [x] Plan/todo tool + verifier loop (docs/plan_todo_verifier/): a
      TodoWrite-style `todo_write` tool over a controller-owned
      `TodoList`, rendered as a `todo: d/t` status segment, plus an
      opt-in (`ANIE_VERIFIER=1`) self-critique `BeforeModelPolicy` that
      injects a one-shot context-only verification nudge when the plan is
      all-done. `ChainedBeforeModelPolicy` composes it with context
      virtualization. Run-scoped/in-memory; parallel sub-agents and DAG
      decomposition deferred.
- [x] MCP (Model Context Protocol) client (docs/mcp_client/): `[mcp]`
      config, hand-rolled stdio JSON-RPC client in the new `anie-mcp`
      crate, external server tools registered as `mcp__<server>__<tool>`
      at bootstrap with log-and-skip graceful failure. Scope is
      client + tools only; resources/prompts/SSE/OAuth deferred.
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

### 0b. Persist a running session usage/cost total (budget-evasion fix)
**What**: Stamp a cumulative usage+cost total into the session log (e.g.
on the compaction entry) and seed the cost meter / budget baseline from
it on resume, instead of re-summing the (compacted) active context at the
current model's rates.
**Why**: Two review-confirmed budget-enforcement gaps (documented in
`budget_policy.rs`): (a) compacting and resuming drops compacted-away
turns from the session total, so `max_session_tokens` /
`max_session_cost_usd` can be evaded; (b) the dollar ceiling re-prices
history at the current model's rate, so a `/model` switch breaks it. Both
are budget-evasion, not display nits.
**Effort**: Medium — a persisted total + seed-on-resume; the per-message
`usage.cost` already exists, stamped at generation time.

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
**Status**: Shipped. `/rewind` + `/checkpoint` (Workstream B), the
interactive `/session` **picker** (Workstream A), and fork/rewind branch
summaries (Workstream C) all landed via docs/session_ux/ — see Completed.
Only per-session display names (`/name`) remain, and the deferred
tree-overlay branch visualization (SESSION-2).
**Details**: [docs/notes/commands_and_slash_menu.md](notes/commands_and_slash_menu.md)
and [docs/session_ux/README.md](session_ux/README.md)

## Longer-Term — Features

### 9. TUI layout improvements
**What**: Input area bars, region separation, user message styling.
**Why**: Visual clarity — the three TUI regions blur together.
**Effort**: Medium-large — layout restructuring, theme tokens.
**Details**: [docs/notes/tui_layout_and_visual_design.md](notes/tui_layout_and_visual_design.md)

### 10. Skills system — **shipped (thin loader)**
Landed via docs/skills_loader/. `SKILL.md` files under `~/.anie/skills/`
and the project `.anie/skills/` are discovered (project precedence),
their `name`/`description`/`allowed-tools` frontmatter parsed by a
hand-rolled subset parser (no YAML dependency), and each registered as a
`/skill:<name>` command surfaced under the `Skills:` group in `/help`.
Invoking `/skill:<name>` stages the skill body as a synthetic user turn
injected ahead of the next prompt (via the session-append seam, not the
single `BeforeModelPolicy` slot). Malformed files warn and are skipped,
never panic. Deferred: hard `allowed-tools` enforcement, auto-injection
into initial context, a `/skills` listing command, and the full
out-of-process Plan-10 extension host.
**Details**: [docs/skills_loader/README.md](skills_loader/README.md),
[docs/notes/skills_system.md](notes/skills_system.md)

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
