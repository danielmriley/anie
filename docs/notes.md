# anie-rs Planning Issue Tracker

This file supersedes the earlier free-form architecture review notes.

It tracks which planning issues have been:
- **resolved in the docs**,
- left as **implementation watchpoints**, or
- intentionally **deferred until after v1.0**.

Use this file as the planning status ledger; use the phase docs and build docs as the source of truth for current behavior.

---

## Resolved in the revised plans

| ID | Topic | Status | Resolution in docs |
|---|---|---|---|
| 1 | Context ownership between agent loop and TUI | Resolved | `AgentLoop::run(...)` now uses owned context and returns `AgentRunResult`; interactive control lives outside `anie-tui`. |
| 2 | `LlmMessage` may be unnecessary | Resolved | Kept intentionally for provider conversion/debugging/testing boundaries. |
| 5 | Only four core tools (`read`, `write`, `edit`, `bash`) | Resolved | Kept intentionally; `grep/find/ls` remain a future convenience feature, not a v1.0 blocker. |
| 6 | JSON Schema validation for tool arguments | Resolved | Phase plans still use schema validation before tool execution. |
| 7 | Session entry ID linking for compaction | Resolved | `SessionContextMessage` preserves `entry_id` alongside each message. |
| 8 | Compaction during active turn | Resolved | Planned both proactively and reactively via overflow recovery. |
| 9 | `AgentEvent` growth over time | Resolved | TUI event handling already uses tolerant matching with ignored variants where appropriate. |
| 12 | RPC mode was underspecified | Resolved | Phase 5 now defines a minimal versioned v1 JSONL protocol with a startup handshake. |
| 13 | No TUI tests | Resolved | Phase 3 now requires `ratatui::TestBackend` snapshots and event-to-render coverage. |
| 14 | `similar` vs `diff` crate | Resolved | Keep `similar`. |
| 15 | File mutation queue path aliasing | Resolved | `FileMutationQueue` canonicalizes paths before locking. |
| 16 | Missing `getSteeringMessages` / `getFollowUpMessages` | Resolved | Added to the Phase 1 agent-loop plan as hooks that can initially return empty vectors. |
| 17 | No `--version` flag | Resolved | Added in the CLI plan. |
| 18 | Consider `--no-tools` | Resolved | Added in the CLI plan. |
| 19 | Status-bar token counts were misleading | Resolved | TUI now prefers provider-reported `input_tokens`, falling back to estimates only when needed. |
| 20 | System prompt size risk from context files | Resolved | Config now includes `max_file_bytes` and `max_total_bytes` caps for project context. |
| 21 | v1 scope drift (Copilot/OAuth quietly becoming part of v1) | Resolved | Phase 2 now has an explicit v1.0 scope guard; Copilot is post-v1.0 and Google is optional stretch. |
| 22 | Sync key lookup incompatible with future auth flows | Resolved | Replaced with async `RequestOptionsResolver` + `ResolvedRequestOptions`. |
| 23 | Provider stream errors lost structure | Resolved | `ProviderStream` now yields `Result<ProviderEvent, ProviderError>`. |
| 24 | TUI crate was taking on orchestration duties | Resolved | Orchestration moved to `anie-cli` / interactive controller; `anie-tui` stays UI-only. |
| 25 | `WriteTool` landed too late for a real coding vertical slice | Resolved | `WriteTool` moved into Phase 1. |
| 26 | Session resume semantics were ambiguous | Resolved | `--resume <session_id>` now resumes the most recently appended leaf in that session file. |

---

## Open implementation watchpoints

These are no longer planning gaps, but they are still places where implementation can go wrong.

| ID | Topic | Why it is still open |
|---|---|---|
| 3 | `serde(tag = "role")` collision risk | Verify during implementation that no inner message structs introduce a conflicting `role` field. |
| 4 | `async_stream` vs manual stream state machine | Start with `async_stream`, but be ready to switch if provider implementations hit lifetime/capture issues. |
| 11 | Thinking budget mapping | The plans keep provider-specific defaults, but local-model reasoning now needs a richer capability model plus later per-model tuning; see `docs/local_model_thinking_plan.md`. |
| 27 | EditTool fuzzy matching semantics | Fuzzy normalization should only help find spans; edits must still be applied to the original normalized buffer, not the fuzzy one. |
| 28 | Session branch UX beyond the active leaf | Storage supports branching, but explicit branch selection / resume-by-leaf is intentionally not in v1.0. |
| 29 | Local-model quirks | Ollama / LM Studio / local OpenAI-compatible servers can differ on usage fields, stream options, function-calling behavior, and reasoning controls/stream shape; see `docs/local_model_thinking_plan.md` and `docs/phased_plan_v1-0-1/`. |

---

## Explicit post-v1.0 deferrals

These items are intentionally out of scope for the first release and should not silently become blockers again.

| Topic | Status |
|---|---|
| GitHub Copilot OAuth | Post-v1.0 |
| Compiled extension system (`anie-extensions`) | Post-v1.0 unless schedule is ahead |
| `memory_write` | Post-v1.0 |
| Guaranteed Google provider support | Optional stretch / likely v1.1 if it slips |
| Fully extensible API kind beyond the built-in enum | Post-v1.x design work |
| Rich branch-selection UX | Post-v1.0 |

---

## Current planning decisions to preserve

These decisions should be treated as deliberate unless there is a conscious re-plan:

1. **Local-first v1.0** — OpenAI-compatible local providers (`ollama`, `lmstudio`) are part of the required development path.
2. **Owned agent context** — no shared mutable transcript between TUI and agent loop.
3. **Structured provider errors** — retries and overflow recovery depend on this.
4. **Interactive controller pattern** — UI renders and emits actions; controller owns orchestration.
5. **Session persistence from run results, not render events** — avoids subtle drift between transcript rendering and canonical history.
6. **Prompt context caps** — large project context files must not silently consume the context window.

---

## How to use this file

- If a planning concern is fully absorbed into the current docs, move it into **Resolved in the revised plans**.
- If it is a real implementation risk, keep it in **Open implementation watchpoints**.
- If it is intentionally not part of the first release, move it into **Explicit post-v1.0 deferrals**.

For shipping status and sequencing, see:
- `docs/v1_0_milestone_checklist.md`
- `docs/IMPLEMENTATION_ORDER.md`
- `docs/phase_detail_plans/`
- `docs/anie-rs_build_doc.md`
