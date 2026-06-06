# anie vs. rival coding agents — gap analysis (2026-06-06)

Goal: make anie a credible rival to Claude Code, OpenAI Codex, and Grok Build.
Produced by a multi-agent analysis: Phase 1 mapped all 12 crates + digested
existing self-assessment docs; Phase 2 graded anie against rivals across 8
competitive lenses and adversarially verified every gap claim against the
actual code. Raw outputs: `phase1_raw.json`, `phase2_raw.json`.

## Calibration corrections to the original brief

The brief assumed several gaps that are in fact **already built** (verified in
Phase 1/2 — do not re-plan these):

- **REPL agent loop** — done. `AgentRunMachine` (Read/Eval/Print/Decide) in
  `anie-agent/src/agent_loop.rs`, with `BeforeModelPolicy` + `CompactionGate`
  trait hooks.
- **Automatic / mid-turn compaction** — done. `anie-cli/src/compaction_gate.rs`
  (stagnation detection, aggressive levels, per-turn budget) + split-turn
  compaction in `anie-session`.
- **Context-length detection** — done for native Ollama (`/api/show`) +
  `/context-length` override.
- **Context virtualization / embedding rerank** — exists on this branch
  (`context_virt.rs`, `Embedder`/`OllamaEmbedder`/cosine — the `rlm/*` work).
- **Sub-agent recursion** — exists (`recurse.rs`, `RecurseTool`) but serial,
  depth 2, budget 8.
- **OAuth for 5 providers**, live model discovery, markdown TUI, SSRF-hardened
  web tools, structured `ProviderError` + `RetryPolicy` — all done.

**Tooling caveat:** the pi reference tree is **not present** on this machine
(`/home/daniel/Projects/agents/pi` missing). `docs/anie_vs_pi_comparison.md` is
the only pi-shape reference available; plans cannot cite live pi file:line.

## Verified gap counts

33 confirmed gaps, 11 partial (scaffolding exists), 4 claims dropped as
already-existing. Full detail in `phase2_raw.json`.

## Ranked initiative shortlist (impact ÷ effort)

| # | Initiative | Impact | Effort | Why it's ranked here |
|---|---|---|---|---|
| 1 | **Tool permission & approval layer** | 5 | 3 | `BeforeToolCallHook`+`Block` already plumbed (`hooks.rs`, `agent_loop.rs:1466`) but only test-wired. Wiring a real permission policy + TUI approval modal + config modes is mostly *integration*. #1 trust differentiator vs a toy. |
| 2 | **MCP client** | 5 | 4 | Zero MCP today. A stdio/SSE MCP client that registers external server tools into `ToolRegistry` unlocks the whole rival tool ecosystem via a standard — sidesteps the bespoke 7-week Plan-10 extension system. |
| 3 | **Plan/Todo tool + verifier loop** | 4 | 3 | No todo/plan tool, no verifier consumer of the `BeforeModelPolicy` seam. Hallmark agentic UX (Claude Code TodoWrite). Cheap; uses existing seam. |
| 4 | **`apply_patch` tool** | 4 | 3 | Only exact-string `edit` today. A structured unified-diff apply tool (Codex's core editing primitive) materially improves multi-hunk/multi-file edits. |
| 5 | **Cost / token-budget enforcement** | 4 | 3 | `Cost` struct + per-model pricing + `Usage` exist but cost is never populated or enforced. Wire usage→cost + per-run/session ceiling. Cheap, high trust value. |
| 6 | **Eval harness + metrics export** | 4 | 4 | No scenario runner, no structured metrics. Without this, "rival-grade" is unfalsifiable. Strategic enabler that makes every other improvement measurable. |
| 7 | **Process sandbox (Landlock/seccomp/seatbelt)** | 4 | 5 | The other half of safety. Expensive, platform-specific. Do *after* the approval layer (#1), which delivers most user-perceived safety. |
| 8 | **Session UX: picker + checkpoint/rewind** | 3 | 4 | `session_picker.rs`/`tree.rs` are stubs; `/fork` exists but unsummarized; no working-tree snapshot/rewind (Claude Code rewind). |
| 9 | **Skills loader (SKILL.md → slash command)** | 3 | 4 | Thin subset of Plan 10. Full bespoke extension system deprioritized in favor of MCP (#2). |

Deprioritized: provider breadth (OpenRouter offsets it; docs already say don't
chase pi's 15 variants), full Plan-10 JSON-RPC extension system (MCP covers most
of its value far cheaper).

## Approved plans (Phase 4 — written, reviewed, revised)

Implementation order (user-approved). Each plan follows the project template and
was adversarially re-verified against the actual code.

| Order | Plan | PRs | Notes |
|---|---|---|---|
| 1 | [`mcp_client`](../mcp_client/README.md) | 6 | New hand-rolled `anie-mcp` crate; **no `rmcp` dep** (deferred until SSE/OAuth needed). |
| 2 | [`plan_todo_verifier`](../plan_todo_verifier/README.md) | 5 | Todo tool + verifier on existing `BeforeModelPolicy` seam; no new dep. |
| 3 | [`apply_patch_tool`](../apply_patch_tool/README.md) | 5 | Codex-style patch envelope via existing `FileMutationQueue`. |
| 4 | [`cost_budget`](../cost_budget/README.md) | 3 | Wire `Usage`→`Cost` (cost is `0.0` today); optional run/session ceilings. |
| 5 | [`eval_harness`](../eval_harness/README.md) | 4 | Scenario runner + JSON metrics; reuses `harness_mode` + `compaction_stats`. |
| 6 | [`tool_sandbox`](../tool_sandbox/README.md) | 5 | New `anie-sandbox` crate; Linux Landlock+seccomp; flags the un-selected approval layer (#1) as companion. |
| 7 | [`session_ux`](../session_ux/README.md) | ~6 | Session picker overlay + working-tree checkpoint/rewind. |
| 8 | [`skills_loader`](../skills_loader/README.md) | 4 | `SKILL.md` → `/skill:name`; full extension system stays deferred. |

The pi source tree being absent means none of these cite live pi file:line; each
uses `docs/anie_vs_pi_comparison.md` as the pi reference and says so.
