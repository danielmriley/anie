# Anie Roadmap

Single source of truth for what's shipped, what's
in flight, and what's next. Each row links to a plan
series or doc; series with their own status trackers
are linked at "see tracker."

Last consolidated: 2026-05-08.

## Active plan series

The major in-flight efforts. Each series owns its own
README + per-PR plan docs. The status column here is
high-level; the per-series tracker is authoritative.

| Series | Goal | Status | Tracker |
|---|---|---|---|
| **RLM + context virtualization** (`rlm_2026-04-29/`) | Recursive Language Models substrate: recurse tool, indexed external store, eviction policy, ledger injection, embedding-based reranker, background summarization | **All 6 phases (A-F) + Plan 08 embedding reranker landed on `dev_rlm`** | [`rlm_2026-04-29/execution/README.md`](rlm_2026-04-29/execution/README.md) |
| **Harness mitigations** (`harness_mitigations_2026-05-01/`) | Fix the loudest small-model failure modes from the 2026-05-01 smoke (hallucinated success on tool error, stuck loops, hallucinated improvements) | **PRs 1-4 shipped on `dev_rlm`. PR 4 landed in two parts: Signal A (supersession-based failure eviction, `2fee147`) and Signal B (position-based stale-failure eviction, `81c817e`)** | [`harness_mitigations_2026-05-01/README.md`](harness_mitigations_2026-05-01/README.md) |
| **Sub-agents + decompose + parallel decomposition** (`rlm_subagents_2026-05-01/`) | Address the long-tail-reasoning gap (T2 stalled at 43 min): true sub-agents with full tools, decompose-and-recurse scaffolding, parallel decomposition (revised from voting after design review) | **PRs 1-5.1 shipped: depth observability, tool inheritance, sub-agent resource stats, one-shot pre-loop decompose with visibility + tuned system prompt, parallel-decompose dry-run (parser + round renderer), and concurrent provider-aware sub-agent executor (`c1bb46a`). PR 6 (smoke validation) ✓ — see smoke_protocol_2026-05-01.md** | [`rlm_subagents_2026-05-01/README.md`](rlm_subagents_2026-05-01/README.md) |
| **Skills system** (`skills_2026-05-02/`) | Anthropic-style skills: markdown files in `.anie/skills/` (and `.agents/skills/`) that the agent loads on demand. The discovery layer for the recurse/decompose capabilities | **PRs 1-4 shipped: registry, skill tool, four bundled skills (cpp-rule-of-five, decompose-multi-constraint-task, use-recurse-for-archive-lookup, verify-after-edit), `/skills` slash command. PR 5 (smoke validation) plan written; smoke run pending** | [`skills_2026-05-02/README.md`](skills_2026-05-02/README.md) |
| **REPL agent loop** (`repl_agent_loop/`) | Refactor `AgentLoop::run` into an explicit Read → Eval → Print → Loop runtime — the substrate that everything above ultimately rides on | **All 7 PRs shipped on `dev_rlm`: characterization tests, `AgentRunState` extraction, internal REPL driver, tracing spans, public `AgentRunMachine` API, controller pilot, `BeforeModelPolicy` hook (default noop)** | [`repl_agent_loop/execution/README.md`](repl_agent_loop/execution/README.md) |
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

- [x] `/goal` autonomous goal loop (docs/goal_command/): `/goal
      <description>` puts the agent into an autonomous "Ralph loop" — it
      works toward the goal, **re-prompts itself to continue after every
      turn** (no interval), verifies its own work, and **stops on a
      completion signal** (`GOAL_COMPLETE` / `GOAL_BLOCKED: <reason>`
      sentinels it emits), a turn cap (`GOAL_MAX_TURNS = 50`), or the
      `[budget]` ceiling. User follow-ups always take priority (the goal
      resumes after them); `/goal stop` cancels; mutually exclusive with
      `/loop`. Modeled on Codex's `/goal`. Continuation reuses the
      run-completion boundary; completion detection is a text sentinel
      (a structured `goal_complete` tool is a deferred refinement).
- [x] `/loop` recurring-prompt command (docs/loop_command/): `/loop
      <Nm|Ns> <message>` re-submits a prompt on a fixed interval so the
      agent keeps working a goal across turns. Modeled on Codex's `/loop`
      (issue #15679): it **queues** rather than interrupts a running turn
      (reusing the existing `QueuePrompt` "start-if-idle / queue-if-busy"
      contract), **replaces** on re-issue, dedups so duplicate loop
      messages don't pile up, and carries a hard iteration cap
      (anie-specific runaway guard; `[budget]` ceilings also still apply).
      `/loop stop` cancels; `/loop` reports status. Timer lives in the
      controller's `select!` (the `PendingRetry` arm's precedent). Self-
      removal on task completion and a status-bar segment are deferred.
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
- [x] **Skills system PRs 1-4** — Anthropic-style skills
      end-to-end: SkillRegistry with six-layer discovery
      (bundled embedded via include_str! + .claude/.agents/.anie
      at user + project), `skill` tool wrapping bodies in
      `<system-reminder source="skill:NAME">`, four bundled
      skills targeting documented failure modes (rule-of-five,
      decompose, recurse-for-archive, verify-after-edit), and
      `/skills` slash command. Catalog appears in the system
      prompt; agent can autonomously load. See
      [`skills_2026-05-02/`](skills_2026-05-02/). PR 5 (smoke
      validation) ✓ in `smoke_protocol_2026-05-01.md`.
- [x] **Sub-agents PRs 1-5.1** — depth observability,
      filtered tool inheritance for sub-agents, per-sub-agent
      resource stats (tokens/wall-clock/cost in
      `result.details`), one-shot pre-loop decompose
      (`ANIE_DECOMPOSE=1`) with plan visibility +
      dependency-marker contract, parallel-decompose
      dry-run (`ANIE_PARALLEL_DECOMPOSE>=2`) that parses the
      plan into a topological round structure, and
      concurrent provider-aware sub-agent executor (PR 5.1,
      `c1bb46a`). Validated end-to-end with the 2026-05-02
      comprehensive smoke. See
      [`rlm_subagents_2026-05-01/`](rlm_subagents_2026-05-01/).
- [x] **REPL agent loop PRs 1-7** — refactor of
      `AgentLoop::run` into an explicit
      Read → Eval → Print → Loop runtime.
      Characterization tests (`2b3f951`) → `AgentRunState`
      extraction (`02cc0cd`) → internal REPL driver
      (`f053013`) → REPL tracing spans (`df07082`) →
      public `AgentRunMachine` API (`f3e3cf7`) → controller
      pilot routed through the machine (`9aedb35`) → first
      `BeforeModelPolicy` policy boundary, default noop
      (`e55948a`). The substrate that future RLM /
      sub-agents / skills capabilities extend through
      structured policy hooks rather than monolithic
      branches in the agent loop. See
      [`repl_agent_loop/execution/README.md`](repl_agent_loop/execution/README.md).
- [x] **TUI typing fast-path** — bypass ratatui for
      printable keystrokes in idle state (`e8927f1`).
      Per-keystroke cost dropped from 22 bytes through
      tokio + render closure + ratatui diff
      (~250 µs/key) to 1 byte direct stdout write
      (vim-comparable). Nine-round investigation logged at
      [`code_review_2026-05-03.md`](code_review_2026-05-03.md);
      decisive insight came from a standalone reproducer
      (`crates/anie-tui/examples/typing_repro.rs`) that
      isolated the latency to anie's pipeline rather than
      the bytes leaving the process.
- [x] **Automatic context compaction** —
      [`midturn_compaction_2026-04-27/`](midturn_compaction_2026-04-27/)
      Plans 01–06 all Done. Compaction now fires
      proactively when token usage crosses the
      context-aware reserve threshold, mid-turn or
      between turns; `CompactionGate` trait sits on the
      agent-loop boundary so the controller decides; tool-
      output caps scale with the live context window;
      `CompactionPhase` events drive `/state` rendering
      and the TUI activity-row labels.
- [x] **Local model context length detection** — three
      coordinated plans:
      [`ollama_capability_discovery/`](ollama_capability_discovery/)
      replaced substring family-name matching with
      authoritative `/api/show` probing (capability flags
      + per-model `context_window`),
      [`ollama_context_length_override/`](ollama_context_length_override/)
      added the `/context-length` slash command for
      persistent per-model user override, and
      [`ollama_default_num_ctx_cap/`](ollama_default_num_ctx_cap/)
      added a workspace-level cap. The Ollama native
      `/api/chat` codepath now sends the discovered
      `num_ctx` per model instead of the old 32K default.
- [x] **Session management commands** — `/name [<name>]`
      to set / clear a persistent user-set display name on
      the current session (`f3652e6`, schema v5
      `SessionHeader.name` with forward-compat), and
      `/resume` to switch to the most-recently-modified
      other session without typing an ID (`e74eb26`).
      `/session list` + `/session <id>` switch were
      already in place; `--resume <id>` CLI flag too.

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

### 1. Automatic context compaction — **shipped**
Landed across the `midturn_compaction_2026-04-27/` plan
series (Plans 01–06, all Done): context-aware reserve,
per-turn budget, agent-loop `CompactionGate` trait,
mid-turn `auto_compact` execution wired into the
controller's main loop, tool-output caps that scale with
the live context window, and compaction telemetry
(`CompactionPhase` events, `/state` rendering, TUI
activity-row phase labels). See
[`midturn_compaction_2026-04-27/execution/README.md`](midturn_compaction_2026-04-27/execution/README.md).

### 6. Local model context length detection — **shipped**
Landed across three coordinated plans:
[`ollama_capability_discovery/`](ollama_capability_discovery/)
(authoritative `/api/show`-driven `Model.context_window`
per model, plus capability flags),
[`ollama_context_length_override/`](ollama_context_length_override/)
(`/context-length` slash command for per-model user
override with persistence), and
[`ollama_default_num_ctx_cap/`](ollama_default_num_ctx_cap/)
(workspace-level `num_ctx` cap for constrained hardware).
The Ollama native `/api/chat` codepath sends the
discovered `num_ctx` on every request rather than the old
32K default.

### 7. Slash command autocomplete menu — **shipped**
Landed via plans 11 and 12. Typing `/` opens a filterable popup
that reads from the `SlashCommandInfo` catalog; argument values
complete for `Enumerated` (e.g. `/thinking`) and `Subcommands`
(e.g. `/session`) specs. Disable with
`ui.slash_command_popup_enabled = false` in `~/.anie/config.toml`.
File-path `@` completion remains a follow-up.

### 8. Session management commands (`/resume`, `/session`, `/name`) — **shipped**
- `/session list` and `/session <id>` switching landed
  earlier (`UiAction::ListSessions`, `SwitchSession`); the
  interactive `/session` **picker**, `/rewind` + `/checkpoint`,
  and fork/rewind branch summaries landed via
  [docs/session_ux/README.md](session_ux/README.md).
- `/name [<name>]` adds a user-set display name on the
  current session (schema v5 `SessionHeader.name`,
  atomic-rewrite persistence, surfaces in `/session list`
  as `name (id)`). Commit `f3652e6`.
- `/resume` switches to the most-recently-modified other
  session — slash-command counterpart to `--resume <id>`
  without typing the ID; refuses cleanly when no siblings
  exist or a run is active. Commit `e74eb26`.
- `--resume <id>` CLI flag was already in place.
- Deferred: tree-overlay branch visualization (SESSION-2).

## Longer-Term — Features

### 9. TUI layout improvements
**What**: Input area bars, region separation, user message styling.
**Why**: Visual clarity — the three TUI regions blur together.
**Effort**: Medium-large — layout restructuring, theme tokens.
**Details**: [docs/notes/tui_layout_and_visual_design.md](notes/tui_layout_and_visual_design.md)

### 10. Skills system — **shipped (merged registry)**
Two parallel implementations were reconciled in the dev_rlm merge.
The surviving loader is the `SkillRegistry` from
[`skills_2026-05-02/`](skills_2026-05-02/): six discovery roots
(`.anie`/`.agents`/`.claude`, project + user) plus bundled skills
embedded via `include_str!`, precedence-based shadowing, YAML
frontmatter (`name`/`description`/`allowed-tools`/
`disable-model-invocation`), and a system-prompt catalog. On top of
it sit both activation paths: the model-invoked `skill` tool and
`/skills` listing (skills_2026-05-02), and per-skill `/skill:<name>`
commands that stage the body as a synthetic user turn ahead of the
next prompt (docs/skills_loader/). Malformed files warn and are
skipped, never panic. Deferred: hard `allowed-tools` enforcement and
the out-of-process Plan-10 extension host.
**Details**: [docs/skills_loader/README.md](skills_loader/README.md),
[docs/skills_2026-05-02/README.md](skills_2026-05-02/README.md)

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
