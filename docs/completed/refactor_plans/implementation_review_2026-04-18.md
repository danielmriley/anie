# Refactor Plan Implementation Review — 2026-04-18

Review of `docs/refactor_plans/00–08` against the working tree on
`refactor_branch` (HEAD `95222d6`). Workspace is green:
`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
and `cargo test --workspace` all pass (320 tests, 0 failures, 1 ignored).

This document calls out **plans that are not fully implemented against
their own exit criteria**, and **areas where the implementation
diverged from the plan in a way that is worth revisiting**. Plans not
listed below are considered complete.

---

## Plan 01 — `openai.rs` module split

**Status in plan:** Phases 1–5 complete. Phase 6 mostly trimmed.

**Gaps:**

1. **Phases 3, 4, 5 — tests not colocated with the submodules.**
   - `openai/tagged_reasoning.rs` correctly has its 12 inline tests.
   - `openai/streaming.rs`, `openai/convert.rs`, and
     `openai/reasoning_strategy.rs` each have **zero** inline
     `#[test]` blocks.
   - All the tests for those submodules still live in
     `openai/mod.rs`'s shared `mod tests` (lines 380–1449 of 1449).
   - Each phase's plan-specified "new file:
     `<submodule>/tests.rs`" bullet was skipped. Plan's own status
     note acknowledges this as "future work, not a prerequisite,"
     but every Phase N exit criterion named colocated tests as a
     requirement.
2. **Phase 6 LOC cap is formally violated.**
   - Production code in `openai/mod.rs` is ~370 LOC (within plan
     target).
   - File length is 1449 LOC because the test block was kept
     centralized. The exit-criterion wording says "`openai/mod.rs`
     ≤ 800 LOC" without qualifying tests-vs-production. Reader can
     reasonably interpret this as unmet.

**Suggested follow-up:** relocate the test module blocks to sit
next to the code they exercise. Worth doing before plan 04 phase 2
(`ToolCallAssembler`) if/when that is revisited — the stream-state
tests will migrate with the code cleanly only if they're colocated
first.

---

## Plan 02 — TUI overlay trait

**Status in plan:** all structural phases landed.

**Gaps:**

1. **Phase 6 Sub-step B — placeholder overlay stubs NOT created.**
   The plan specified that `session_picker.rs`, `settings.rs`,
   `oauth.rs`, `theme_picker.rs`, `hotkeys.rs`, and `tree.rs` each
   land as compiling `impl OverlayScreen` stubs rendering
   "not yet implemented."
   - Actual state: `crates/anie-tui/src/overlays/` contains only
     `mod.rs`, `model_picker.rs`, `onboarding.rs`, `providers.rs`.
   - `overlays/mod.rs:12` even documents the opposite approach
     ("land here as they're implemented"), directly contradicting
     the plan's rationale for Phase 6.
   - Exit criterion explicitly named: "Placeholder stubs exist
     for `session_picker`, `settings`, `oauth`, `theme_picker`,
     `hotkeys`, `tree`." **Unmet.**

   The plan's motivation for this sub-step was that pre-placing
   stubs is cheap and prevents re-migrating overlays into
   `overlays/` as they're added. The design intent is now lost —
   the next overlay added will have no scaffold to land against.

2. **Phase 4 — clone audit partial.** Status note deferred two
   items:
   - `self.state.clone()` in `onboarding::render` at
     `overlays/onboarding.rs:293`. Runs once per frame.
   - `HashMap<String, TestResult>` in `overlays/providers.rs:133,
     181, 297`. Never migrated to `HashMap<usize, TestResult>` or
     `ProviderId` newtype.
   Each defer is individually defensible (documented in the plan
   status), but the Phase 4 exit criteria remain formally unmet.

**Suggested follow-up:** land Phase 6 Sub-step B. Six one-file
stubs totaling ~200 LOC is a half-hour job and it directly blocks
the feature-additions listed in `docs/ideas.md` from having a
consistent landing pad.

---

## Plan 03 — Controller decomposition

**Status in plan:** "all five phases landed."

This is the plan with the **largest divergence from its stated
goals**.

**Gaps:**

1. **Phase 3 was substantially downgraded.**
   - Plan called for a `trait SlashCommand` with `async fn dispatch`,
     a `commands/builtin.rs` with per-command impls, and a
     dispatch-based `CommandRegistry` that replaces `handle_action`'s
     20-arm match.
   - What was built: `commands.rs` is a metadata-only registry
     (`SlashCommandInfo` = name + summary + source). No trait. No
     handler impls. `handle_action` still contains the flat
     20-arm match (see `controller.rs:408` onward).
   - Five of the registry's seven public methods carry
     `#[allow(dead_code)]` comments saying they'll be used "once
     `/help` lands" — meaning the registry is not actually driving
     any behavior today. It's a placeholder with tests.
   - Exit criteria explicitly unmet:
     - "handle_action contains no slash-command match arms"
     - "Adding a new /settings or /copy command is: write a
       SlashCommand impl, register in CommandRegistry::with_builtins()"
     - "Extensions (plan 10 phase 4) can register commands with a
       non-Builtin source; registry accepts and exposes them" —
     technically true for registration, but since dispatch doesn't
     consult the registry, registration is a no-op at runtime.
   - The status note defends this as "pi's own slash-commands.ts
     also keeps dispatch separate from metadata." That is
     accurate, but pi nonetheless wires `/help` and autocomplete
     through the metadata. anie's current `/help` is still the
     hard-coded one.

2. **Phase 4 decision logic not extracted.**
   - `RetryConfig` and `retry_delay_ms` moved to
     `retry_policy.rs` (67 LOC).
   - The actual decision logic
     (`schedule_transient_retry`, `retry_after_overflow`,
     `should_retry_transient`) stays inline in
     `controller.rs`. The planned pure
     `RetryPolicy::decide(error, attempt, already_compacted) ->
     RetryDecision` does not exist.
   - The 7 planned unit tests for retry decisions (`auth gives
     up`, `rate limit returns retry`, `context overflow
     compacts`, etc.) do not exist. These are the exact tests
     that would have validated the `ProviderError::is_retryable()`
     shortcut added in plan 05.

3. **Phase 5 is incomplete.**
   - `SessionHandle` and `SystemPromptCache` (renamed from the
     plan's `ConfigState`) were extracted into
     `crates/anie-cli/src/runtime/`.
   - **`ConfigState` itself was NOT extracted.** `ControllerState`
     still carries `config: AnieConfig`, `runtime_state:
     RuntimeState`, `current_model: Model`, `current_thinking:
     ThinkingLevel`, `cli_api_key: Option<String>` as bare fields —
     these were supposed to coalesce into one `ConfigState` handle.
   - `ControllerState` has **13 fields** today, not the 7
     composition handles the plan sketched.
   - `controller.rs` is **1769 LOC**. Phase 5 exit criterion:
     "**`controller.rs` is under 1000 LOC**." Unmet by a wide margin.

**Suggested follow-up:**

- **High priority:** either actually land the Phase 3 dispatch
  refactor (wire the registry into `handle_action` and delete the
  match), or formally rewrite Phase 3 in the plan doc to document
  that only metadata tagging was adopted. Today the plan claims
  victory on something that wasn't done, and `commands.rs` is
  ~65% `#[allow(dead_code)]` scaffolding waiting for a `/help`
  that never shows up.
- **Medium priority:** extract `ConfigState` (or revise Phase 5
  in the plan). The stated goal of "ControllerState has zero
  'doing logic' methods longer than ~20 lines" is credible
  only after this.
- **Medium priority:** build the `RetryPolicy::decide` function
  and its unit tests. Having `ProviderError::is_retryable()` as a
  method on the error type (see plan 05 deviation below) only
  makes sense if the decision layer consumes it — today the
  answer is still derived inline.

---

## Plan 05 — Provider error taxonomy

**Status in plan:** complete.

The migration is solid. One design deviation worth flagging:

- **Design Principle 2 says "Retryability is not in the error type.
  It's a property derived by `RetryPolicy`."**
  - Implementation added `ProviderError::is_retryable()` and
    `ProviderError::retry_after_ms()` as methods directly on the
    enum (`error.rs:94–118`).
  - This is a pragmatic choice — the methods are well-commented
    and the branching is correct — but it conflicts with the
    plan's own design statement, and it collides with plan 03
    Phase 4's unbuilt `RetryPolicy`.

**Suggested follow-up:** either update the plan doc to drop the
principle, or when plan 03 Phase 4 is finished, delete the
retryability methods from `ProviderError` and move the logic into
`RetryPolicy::decide`.

---

## Plan 06 — Session write locking

**Status in plan:** not annotated; spot-check confirms Phases 1–3
landed and the CLI surfaces `AlreadyOpen` cleanly.

**Gap:**

1. **Phase 4 doc update partial.** README was updated
   (`README.md:242`). `docs/arch/anie-rs_architecture.md` was
   **not** updated to mention the advisory-lock behavior. Grep
   for `lock`/`advisory` in that doc returns only the existing
   "Sandboxing" row.

**Suggested follow-up:** add a one-paragraph note under the
session-persistence section of the architecture doc.

✅ Completed via `fixes/06_07_architecture_doc_refresh.md`.

---

## Plan 07 — Extensions stub removal

**Status in plan:** not annotated in the plan file.

Crate was deleted cleanly (confirmed: no source references to
`anie_extensions` anywhere in `crates/`). Two gaps remain:

1. **Phase 1 Sub-step C — architecture doc NOT updated.**
   `docs/arch/anie-rs_architecture.md` still shows:
   - `anie-extensions` box in the diagram (line 40).
   - `anie-extensions` hook points in the event flow (lines 113,
     142, 150).
   - `anie-extensions` in the crate list (line 170).
   `docs/arch/anie-rs_build_doc.md` likely has similar stale
   references (contains the string per grep).

2. **Phase 2 — hooks.rs traits NOT made `pub(crate)`.**
   - Module doc now points to plan 10 ✅.
   - But `BeforeToolCallHook`, `AfterToolCallHook`,
     `BeforeToolCallResult`, and `ToolResultOverride` are still
     declared `pub`. Every usage site in the workspace is inside
     `anie-agent` itself — `pub(crate)` would suffice.
   - Exit criterion "hooks.rs traits are pub(crate)" unmet.

**Suggested follow-up:** both are small edits. Doing them is the
cheapest way to stop misleading code readers — right now the
architecture doc is promising an extension system that doesn't
exist, exactly what the plan was written to prevent.

✅ Completed via `fixes/06_07_architecture_doc_refresh.md` and
`fixes/07_hooks_visibility.md`.

---

## Plan 08 — Small hygiene items

**Status in plan header is stale:** the header says Phase B ("Not
landed — plan 04 phase 1 provides the better fix") and Phase D
("Not landed. Queued"), but git log shows both landed in
`d51672b` ("Plan 08 phases B & D: HTTP fallback + send_or_warn
helper"), and the `send_event` helper in `agent_loop.rs:23–30` is
used 60 times across the controller and agent loop.

**Suggested follow-up:** update the status block in
`08_small_hygiene_items.md` so future readers know Phases B and D
are done.

✅ Completed via `fixes/08_status_hygiene_and_tests.md`.

Functional issue observed: the helper was renamed from
`send_or_warn` (plan) to `send_event` (implementation) with a
process-global latch instead of a per-call `AtomicBool`. That is
arguably better (one warn per process beats one warn per
channel), but the plan's unit tests (3 listed) were not added.

✅ Direct `send_event` log-latch tests were backfilled in
`fixes/08_status_hygiene_and_tests.md`.

---

## Cross-cutting observations

1. **`docs/arch/anie-rs_architecture.md` is broadly stale.**
   Plans 06 and 07 each called for updates to it and neither
   landed them. This doc is the first thing a new contributor
   reads, and it currently describes a crate (`anie-extensions`)
   that doesn't exist and a file-lock model it doesn't mention.
   Worth a dedicated ~30-minute refresh pass against the current
   tree.

2. **Phase status blocks drift from reality.** Plan 08's header
   claims Phases B and D are not landed despite both being
   committed. Plan 03's header claims "all five phases landed"
   despite Phase 3's dispatch refactor and Phase 5's
   `ConfigState` extraction not landing. Phase-status blocks are
   useful only if accurate — either remove them or treat them as
   a required edit when a phase finishes.

3. **Dead-code scaffolding.** Plan 03 Phase 3's
   `commands.rs` exports a registry whose five non-constructor
   methods all carry `#[allow(dead_code)]` "used once /help
   lands" comments. This is the sort of thing CLAUDE.md's
   "delete before you build" directive warns about: if `/help`
   does not actually consume the registry today, the registry is
   speculative design for a future that may not arrive in its
   current shape. Either wire it up now (even partial, just for
   `/help`) or trim the unused surface area.

4. **Plan 01 tests-vs-code file-size hygiene.** The decision to
   keep the test module in the same file as the production code
   is fine in isolation, but it complicates reading diffs in the
   file (a 50-line change to the Provider impl reads as a
   change to a 1449-line file). Colocating tests with their
   submodules is the standard Rust idiom and the plan already
   called for it — worth finishing.

## What's in good shape

- Plan 00 (CI) is exactly as specified.
- Plan 04 Phase 1 (shared HTTP client) and Phase 3 (unified
  discovery) are clean; the Phase 2 deferral has sound rationale
  and is documented in the plan.
- Plan 05 migration is thorough: no lingering `ProviderError::Other/Stream/Request/Response`
  in `crates/`; `is_native_reasoning_compatibility_error` uses a
  typed match.
- Plan 06 Phases 1–3 landed the behavior completely; only the
  architecture-doc note is missing.
- Plan 08 Phases A, E, F are all cleanly implemented; Phase C
  correctly leveraged the workspace-level
  `cfg_attr(test, allow(clippy::expect_used))` rather than site-by-site
  comments.
