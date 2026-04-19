# Remaining-work notes (2026-04-18)

Private working notes kept so the next session doesn't re-litigate
the same scoping questions.

## What's actually stopping each deferred plan

Nothing architecturally. All deferrals are scope / context-budget
decisions, not real blockers. No pending design decisions from the
user.

### Plan 03 phase 2 — compaction → `anie-session` behind `MessageSummarizer`

- **Work shape:** introduce `trait MessageSummarizer` in
  `anie-session`; change `SessionManager::auto_compact` /
  `force_compact` to take `&dyn MessageSummarizer` instead of
  `&ProviderRegistry`; build a `CompactionStrategy` in `anie-cli`
  that wraps `ProviderRegistry` + current `Model` + `StreamOptions`;
  collapse the three controller call sites
  (`maybe_auto_compact`, `force_compact`, `retry_after_overflow`)
  which share ~80% of code.
- **Risk:** `anie-session` public API change. Inside the workspace
  only the controller calls these, so blast radius is contained.
- **Status:** queued as next work.

### Plan 03 phase 3 — slash-command registry with `SlashCommandSource` tagging

- **Work shape:** `handle_action` in `controller.rs:408` has ~20
  match arms. Two approaches:
  - **(A)** `Box<dyn SlashCommand>` trait registry with a
    `CommandContext<'_>` pass-through — cleaner, invents a shape.
  - **(B)** Inherent methods retained, registry is
    `HashMap<&'static str, fn(&mut InteractiveController, ...) ->
    BoxFuture<...>>`. Lower-risk, smaller diff.
- **Decision:** pick (B). Mostly mechanical. Introduces the
  `SlashCommandSource` / `CommandInfo` types for future extension
  / prompt / skill registrations.
- **Status:** queued after phase 2.

### Plan 03 phase 5 — recompose `ControllerState`

- **Blocked on:** phases 2 and 3 landing first.
- **Work shape:** once 2 and 3 are in, `ControllerState` should
  decompose into `ModelCatalog` (already a module), `SessionHandle`,
  `ConfigState`, `CompactionStrategy` (from phase 2), `RetryPolicy`
  (already a module), cached `Arc<ToolRegistry>` (already done).
  Phase 5 is the final "put it together" cleanup.

### Plan 05 — provider error taxonomy

- **Must land as one commit.** Phase 1 redesigns the enum (delete
  `Other`, split `Stream(String)` into `EmptyAssistantResponse`,
  `InvalidStreamJson`, `MalformedStreamEvent`, `ToolCallMalformed`,
  `NativeReasoningUnsupported`; split `Request` into `RequestBuild`
  and `Transport`). Phases 2–4 migrate 55 `ProviderError::*`
  construction sites across 11 files. Phase 5 updates callers
  (`retry_delay_ms`, `is_native_reasoning_compatibility_error`,
  tests) to match on variants instead of string `.contains(...)`.
- **Why one commit:** partial migration breaks the build — every
  call site breaks on the removed `Other` variant until migrated.
- **Estimate:** ~2 hours focused; wide but mechanical.

### Plan 08 phases B, D, F — smaller hygiene

- **Phase B (HTTP Result propagation):** covered by plan 04 phase 1
  (shared client already uses `OnceLock<Result<...>>`).
  `create_http_client` still panics but is now a cold-path
  fallback. Could be deleted entirely once all callers migrate to
  `shared_http_client()?`.
- **Phase D (event-send logging):** 22 `let _ = event_tx.send(...)`
  sites across agent_loop + controller. Straightforward to add a
  `send_or_warn` helper with a `OnceLock<()>` "warn once per run."
- **Phase F (borrowing context API):** add `Session::iter_context()`
  on top of existing `build_context()`; migrate the count-only
  call sites.

All three are independent, small, and can be picked up any time.

### Plan 02 phase 5 — overlay integration tests

- 7 onboarding + 6 provider-management tests per the plan. No
  blockers; just new test code to write.

## Execution order for this session

1. **Plan 03 phase 2 (compaction → session).** Higher architectural
   value, cleaner to reason about in isolation.
2. **Plan 03 phase 3 (slash-command registry).** Independent of
   phase 2; can land right after.

Phase 5 waits for a follow-up session.

## Things NOT in scope this session

- Plan 05 (error taxonomy) — separate session.
- Plan 08 phases B/D/F — opportunistic.
- Plan 02 phase 5 — opportunistic.
- Plan 10 (extension system) — explicitly out of scope.
