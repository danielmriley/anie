# anie — Status Report

**Date:** 2026-04-15
**Workspace test status:** 165 tests pass across 11 crates
**Commit:** `0b8fb9b` on `main`

---

## Executive summary

The anie project is in strong shape. The core architecture follows the design documents closely, separation of concerns is well maintained, and the codebase is structured for extensibility. All 165 workspace tests pass. CI and secret scanning are in place.

There are a small number of open items from the implementation plans that have not yet been addressed, and a few areas where the codebase would benefit from additional attention. None are blockers, but several are worth prioritizing.

---

## What is being done well

### 1. Architecture follows the design documents

The original implementation order (`docs/completed/IMPLEMENTATION_ORDER.md`) defined five global guardrails. The codebase adheres to all five:

| Guardrail | Status |
|---|---|
| Owned context: `AgentLoop::run(...)` takes owned context, returns `AgentRunResult` | ✅ Verified |
| Structured provider errors: `Result<ProviderEvent, ProviderError>` | ✅ Verified |
| Async request-option resolution via `RequestOptionsResolver` trait | ✅ Verified |
| UI/orchestration split: `anie-tui` is UI-only, `anie-cli` owns orchestration | ✅ Verified |
| Session persistence from controller/run results, not render events | ✅ Verified |

The dependency graph enforces these boundaries at compile time:
- `anie-tui` depends only on `anie-protocol` — it cannot import session, config, auth, or agent logic
- `anie-agent` depends only on `anie-provider` and `anie-protocol`
- `anie-cli` is the only crate that imports `anie-tui`, and it does so only for the interactive controller

There are no reverse dependencies or layering violations.

### 2. Separation of concerns is clean

Each crate has a focused responsibility:

- **Protocol types** (`anie-protocol`) — 526 lines, 22 tests. Pure data types with no behavior dependencies.
- **Provider abstraction** (`anie-provider`) — 725 lines, 9 tests. Traits and types only; no HTTP or provider-specific logic.
- **Built-in providers** (`anie-providers-builtin`) — 3,359 lines, 41 tests. All provider-specific behavior (Anthropic, OpenAI, local detection, SSE, reasoning strategies) lives here.
- **Agent loop** (`anie-agent`) — 1,870 lines, 13 tests. Tool execution, streaming orchestration, and hook dispatch. No provider or UI knowledge.
- **Tools** (`anie-tools`) — 1,909 lines, 24 tests. Self-contained tool implementations with no knowledge of providers or sessions.
- **Session** (`anie-session`) — 1,474 lines, 9 tests. Append-only JSONL persistence, compaction, branching. No UI or provider knowledge.
- **Config** (`anie-config`) — 649 lines, 7 tests. TOML loading, merging, and project-context discovery.
- **Auth** (`anie-auth`) — 301 lines, 4 tests. Credential storage and resolution.
- **TUI** (`anie-tui`) — 2,828 lines, 25 tests. Pure rendering and input handling.
- **CLI** (`anie-cli`) — 2,188 lines, 11 tests. Controller, onboarding, and mode dispatch.

The controller (`anie-cli`) is the only integration point that wires all layers together. No other crate crosses boundaries.

### 3. The v1.0.1 local-reasoning work landed cleanly

The v1.0.1 phased plan defined 9 steps (Step -1 through Step 7). Based on the existing review (`docs/completed/v1-0-1_review.md`) and code inspection:

| Step | Status |
|---|---|
| Step -1: OpenAI local compatibility hotfixes | ✅ Complete (including empty-stop protection, which was added after the review) |
| Step 0: TUI transcript scrolling and navigation | ✅ Complete |
| Step 1: System-prompt insertion point | ✅ Complete |
| Step 2: Local defaults and prompt steering MVP | ✅ Complete |
| Step 3: Tagged reasoning stream parsing MVP | ✅ Complete |
| Step 4: Capability model and config | ✅ Complete |
| Step 5: Native reasoning controls for modern local backends | ✅ Complete |
| Step 6: Native separated reasoning output | ✅ Complete |
| Step 7: Backend profiles, token budgets, validation | ✅ Complete |
| TUI thinking presentation follow-up | ✅ Complete |

Reasoning behavior lives entirely in the provider layer, as the plan required. The controller still owns only `ThinkingLevel`. The TUI was not touched for reasoning concerns until the presentation follow-up, which was correctly scoped as a pure visual change.

### 4. The v1.0.1 review fixes were addressed

The three issues raised in `docs/completed/v1-0-1_review.md` have all been addressed:

- **Fix 1 — Empty-stop protection:** `finish_stream()` now returns a `ProviderError::Stream("empty assistant response")` when no meaningful content was accumulated. Test: `truly_empty_successful_stop_becomes_stream_error`.
- **Fix 2 — Token headroom direction:** `effective_max_tokens()` implements the "shrink within budget" strategy. The existing test covers the behavior.
- **Fix 3 — Doc naming alignment:** noted for documentation update (see open items below).

### 5. Test coverage is substantial

165 tests across 11 crates. Coverage is strongest in:
- Provider-layer reasoning logic (41 tests in `anie-providers-builtin`)
- TUI rendering and navigation (25 tests in `anie-tui`)
- Tool execution (24 tests in `anie-tools`)
- Protocol serde roundtrips (22 tests in `anie-protocol`)
- Agent loop behavior (13 tests in `anie-agent`)

### 6. Extensibility surface is well prepared

The codebase is structured for future extension:
- `Provider` trait allows new providers without touching existing code
- `Tool` trait allows new tools without touching the agent loop
- `RequestOptionsResolver` trait allows different auth strategies
- `BeforeToolCallHook` / `AfterToolCallHook` traits exist for tool-execution interception
- `ProviderRegistry` and `ToolRegistry` use trait objects for runtime flexibility
- `ApiKind` enum is the only hard-coded provider dispatch point

### 7. CI and repository hygiene

- Cross-platform CI (Ubuntu, macOS, Windows)
- Secret scanning via Gitleaks with a targeted allowlist
- Clean `.gitignore` covering build output, editor clutter, and local runtime files
- README documents the full project

---

## What needs attention

### Priority 1 — Open items from plans

#### 1a. `anie-extensions` is a placeholder

The crate contains only a constant. The design documents (`docs/completed/IMPLEMENTATION_ORDER.md`, phase 5 plan) call for an `Extension` trait with hooks for `before_agent_start`, `session_start`, and `before/after_tool_call`. The agent loop already has `BeforeToolCallHook` and `AfterToolCallHook` traits in `anie-agent/src/hooks.rs`, but there is no extension runner, no extension loading, and no integration in `anie-extensions`.

**Impact:** Low risk today — the hook traits exist and work — but the empty crate signals unfinished work to anyone reading the workspace.

**Recommendation:** Either implement a minimal extension surface or explicitly document that `anie-extensions` is reserved for post-v1.0 and update the README accordingly.

#### 1b. Doc naming alignment (`docs/completed/local_model_thinking_plan.md`)

The design reference doc still uses the old planned enum names (`NativeOpenAiReasoning`, `PromptOnly`, `PromptWithTags`, `NativeDeltas`, `TaggedText`). The implementation uses cleaner names (`Native`, `Prompt`, `Separated`, `Tagged`). This was called out in the v1.0.1 review as Fix 3 but the doc has not been updated.

**Impact:** Causes confusion when reading the design doc alongside the code.

**Recommendation:** Update `docs/completed/local_model_thinking_plan.md` to match the implemented types.

#### 1c. `effective_max_tokens` lacks a doc comment

The v1.0.1 review (Fix 2) recommended adding a doc comment to `effective_max_tokens()` to explain the "shrink within budget" policy. The function currently has no documentation.

**Recommendation:** Add a brief doc comment explaining the intent.

### Priority 2 — Separation of concerns refinements

#### 2a. Controller size and density

`crates/anie-cli/src/controller.rs` is the largest single file in the project at ~1,800 lines. It handles:
- interactive mode orchestration
- print mode orchestration
- RPC mode orchestration
- model resolution
- system prompt construction
- retry/backoff logic
- session management wiring
- compaction trigger logic
- UI action dispatch

While the controller is architecturally correct (it is the integration point), the file would benefit from being split into focused modules (e.g., `interactive.rs`, `print.rs`, `rpc.rs`, `model_resolution.rs`, `system_prompt.rs`).

**Impact:** Readability and maintainability. No architectural violation.

#### 2b. Session tests are sparse relative to complexity

`anie-session` has 9 tests for 1,474 lines of code covering: append-only JSONL persistence, tree-structured branching, compaction, context rebuilding, forking, and session listing. The tests cover the core paths but edge cases around branch navigation, concurrent writes, and malformed-file recovery could use more coverage.

**Impact:** Medium — session persistence is critical infrastructure.

### Priority 3 — Extensibility gaps

#### 3a. `ApiKind` is a closed enum

Adding a new provider API kind (e.g., Google Generative AI, which is already declared as `GoogleGenerativeAI` but has no implementation) requires modifying the `ApiKind` enum in `anie-provider`. This is a compile-time change.

The design docs explicitly note this as a post-v1.0 item ("fully extensible API-kind model beyond the built-in enum"), so this is not a violation. But it is worth noting as the primary extensibility ceiling.

#### 3b. No integration test layer

All tests are unit tests inside their respective crates. There are no integration tests in `tests/` directories that exercise multi-crate flows (e.g., controller → agent → mock provider → session persistence).

The TUI tests come closest by exercising `App` with `AgentEvent` sequences, but they don't cross the controller boundary.

**Impact:** Cross-crate integration issues could go undetected.

### Priority 4 — Documentation drift

#### 4a. v1.0 milestone checklist is unchecked

~~`docs/v1_0_milestone_checklist.md` contained unchecked items~~ — **Resolved:** checklist has been updated and moved to `docs/completed/v1_0_milestone_checklist.md`.

#### 4b. Phase detail plans reference older patterns

Some of the original phase plans in `docs/completed/phase_detail_plans/` contain code snippets and type names that have evolved during implementation (e.g., `ThinkingLevel` as enum variants vs. the actual implementation). These are historical reference docs, but new contributors could be confused.

**Recommendation:** Add a note at the top of each phase plan indicating it is a historical planning document and directing readers to the actual code.

---

## Summary scorecard

| Area | Score | Notes |
|---|---|---|
| Design adherence | ★★★★★ | All global guardrails are respected. Plan steps are implemented. |
| Separation of concerns | ★★★★☆ | Clean crate boundaries. Controller could be split further. |
| Extensibility | ★★★★☆ | Trait-based extension points are well designed. Extensions crate is empty. |
| Test coverage | ★★★★☆ | 165 tests. Strong in providers and TUI. Sessions and integration tests could grow. |
| Documentation | ★★★☆☆ | README is good. Design docs have naming drift and unchecked checklists. |
| CI and repo hygiene | ★★★★★ | Cross-platform CI, secret scanning, clean `.gitignore`. |
| Code quality | ★★★★★ | Consistent style, workspace lints, `cargo fmt`, no warnings. |

---

## Recommended next actions (in priority order)

1. ~~**Update the v1.0 milestone checklist**~~ — Done
2. **Update `docs/completed/local_model_thinking_plan.md`** — align enum names with the implementation
3. **Add a doc comment to `effective_max_tokens()`** — clarify the shrink-within-budget policy
4. **Decide on `anie-extensions`** — implement a minimal surface or explicitly defer and document
5. **Split `controller.rs`** — break into focused modules for readability
6. **Add integration tests** — at minimum, a controller → agent → mock provider → session flow
7. **Expand session tests** — cover edge cases in branching, malformed files, and concurrent access
8. **Add historical-doc notices** — mark the original phase plans as historical references
