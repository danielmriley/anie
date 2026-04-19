# Fix 03c — Finish the controller split (plan 03 phase 5)

Closes out plan 03 phase 5 — extract `ConfigState`, shrink
`controller.rs` below the original 1000-LOC target, and leave
`ControllerState` as a composition of a small number of focused
handles.

## Motivation

Plan 03 phase 5 set three exit criteria:

- `controller.rs` ≤ 1000 LOC.
- `ControllerState` has zero "doing logic" methods longer than ~20
  lines.
- Each extracted type lives in its own file.

Reality on `refactor_branch`:

- `controller.rs` is **1769 LOC**.
- `ControllerState` has **13 fields** (plan envisioned 7 handles).
- `SessionHandle` and `SystemPromptCache` were extracted, but
  `ConfigState` (the plan's third handle) was not; `AnieConfig`,
  `RuntimeState`, `current_model`, `current_thinking`, and
  `cli_api_key` remain as bare fields on `ControllerState`.

The current shape works, but the missed goal means:

- `reload_config`, `apply_session_overrides`, `persist_runtime_
  state`, and `refresh_system_prompt_if_needed` all read and mutate
  scattered fields. Refactoring any of them requires crossing
  three or four field boundaries.
- New features from `docs/ideas.md` (hot-reload of AGENTS.md,
  scoped models per branch, session-level thinking override) need a
  clear place to land. Today each would add more scattered fields.
- `controller.rs` is a 1700-line file again — exactly the shape
  plan 03 was written to prevent.

## Design principles

1. **One concern per handle.** `ConfigState` owns the configured
   knobs (static config + per-session overrides + persisted runtime
   state). `SessionHandle` owns the current session. `ModelCatalog`
   operations stay as free functions today — no need to wrap them.
2. **Delegation, not re-implementation.** `ControllerState`'s
   methods shrink to 1–5 lines of delegation.
3. **Event emission stays with the controller.** Handles return
   data; the controller decides which events to emit. This keeps
   handles pure and testable.
4. **File discipline.** Keep `controller.rs` ≤ 1000 LOC by the end.
   If tests push it over, colocate runtime handle tests with the
   handles themselves.

## Preconditions

- Plan 03 phases 1, 2, 4 landed.
- Fix 03b (RetryPolicy::decide) landed. Without it, the retry-
  related methods still contain decision logic that belongs in
  `RetryPolicy` — cleaning that up first means `ControllerState`
  only has delegator methods left.
- `SessionHandle` and `SystemPromptCache` exist.

---

## Phase 1 — Extract `ConfigState`

**Goal:** Move `AnieConfig`, `RuntimeState`, `current_model`,
`current_thinking`, `cli_api_key` into a single `ConfigState`
handle with the methods that read/write them.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/runtime/config_state.rs` | New — `ConfigState` struct + methods |
| `crates/anie-cli/src/runtime/mod.rs` | Re-export `ConfigState` |
| `crates/anie-cli/src/controller.rs` | Replace the five bare fields with a single `config: ConfigState`; migrate reads and writes |

(3 files; under the cap.)

### Sub-step A — Shape of `ConfigState`

```rust
pub(crate) struct ConfigState {
    anie_config: AnieConfig,
    runtime_state: RuntimeState,
    current_model: Model,
    current_thinking: ThinkingLevel,
    cli_api_key: Option<String>,
}

impl ConfigState {
    pub(crate) fn new(
        anie_config: AnieConfig,
        runtime_state: RuntimeState,
        current_model: Model,
        current_thinking: ThinkingLevel,
        cli_api_key: Option<String>,
    ) -> Self { ... }

    // Accessors
    pub(crate) fn current_model(&self) -> &Model { &self.current_model }
    pub(crate) fn current_thinking(&self) -> ThinkingLevel { self.current_thinking }
    pub(crate) fn anie_config(&self) -> &AnieConfig { &self.anie_config }
    pub(crate) fn cli_api_key(&self) -> Option<&str> { self.cli_api_key.as_deref() }

    // Mutations
    pub(crate) fn set_model(&mut self, model: Model);
    pub(crate) fn set_thinking(&mut self, level: ThinkingLevel);
    pub(crate) fn apply_session_overrides(&mut self, session_overrides: &SessionOverrides);

    // Persistence
    pub(crate) fn persist_runtime_state(&mut self);

    // Reload
    pub(crate) fn reload_from_disk(
        &mut self,
        cwd: &Path,
        provider_override: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<ReloadOutcome>;
}

pub(crate) struct ReloadOutcome {
    pub new_model: Model,
    pub new_thinking: ThinkingLevel,
    pub provider_registry_changed: bool,
}
```

The `ReloadOutcome` return lets `ControllerState::reload_config`
decide which events to emit.

### Sub-step B — Move methods

From `controller.rs`:

| Method | Target |
|---|---|
| `persist_runtime_state` (1014) | `ConfigState::persist_runtime_state` |
| `apply_session_overrides` (989) | `ConfigState::apply_session_overrides` |
| `reload_config` (1024) | `ConfigState::reload_from_disk` |

`set_model`, `set_model_resolved`, `set_thinking` currently do
**two** things: mutate `ControllerState` + append a session event.
Keep the session append in `ControllerState` (it needs `session:
SessionHandle`), but call through to `ConfigState::set_model` for
the field mutation:

```rust
// ControllerState
async fn set_model_resolved(&mut self, model: Model) -> Result<()> {
    upsert_model(&mut self.model_catalog, &model);
    self.config.set_model(model);
    self.session.inner_mut().append_model_change(
        &self.config.current_model().provider,
        &self.config.current_model().id,
    )?;
    self.config.persist_runtime_state();
    Ok(())
}
```

### Sub-step C — Migrate reads

Every `self.current_model` becomes `self.config.current_model()`.
Every `self.current_thinking` becomes `self.config.current_thinking()`.
Every `&self.config` becomes `self.config.anie_config()`.

This is ~30–50 call-site edits. A search-replace with careful
review per hit is sufficient.

### Sub-step D — Keep `model_catalog` outside for now

`model_catalog: Vec<Model>` stays on `ControllerState`. It's
logically part of config but lives separately so plans 03 phase 1
and fix 03a don't have to re-migrate it. If a later plan wants to
bundle it inside `ConfigState`, fine — not this plan.

### Test plan

| # | Test |
|---|------|
| 1 | New `config_state.rs` tests: `persist_runtime_state_writes_expected_fields`, `reload_from_disk_swaps_model_without_changing_session`, `apply_session_overrides_updates_current_model_and_thinking` |
| 2 | Existing controller tests pass without changes |
| 3 | `cargo clippy --workspace --all-targets -- -D warnings` passes |

### Exit criteria

- [ ] `ConfigState` exists in `runtime/config_state.rs`.
- [ ] `ControllerState` no longer holds `config: AnieConfig`,
      `runtime_state: RuntimeState`, `current_model`,
      `current_thinking`, `cli_api_key` as bare fields.
- [ ] `ControllerState` holds `config: ConfigState` instead.
- [ ] All existing tests pass.

---

## Phase 2 — Slim `ControllerState` methods to delegators

**Goal:** Every remaining method on `ControllerState` is either
(a) a 1–5 line delegator to a handle, or (b) an event-emission
coordinator that wires a handle's result into `AgentEvent`
channels.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/controller.rs` | Rewrite `ControllerState` methods as delegators; inline short helpers that only have one caller |

### Sub-step A — Audit current methods

After phase 1 + fix 03b, `ControllerState` has roughly these
methods (drop any that were removed by fix 03b):

| Method | Post-fix shape |
|---|---|
| `set_model`, `set_model_resolved` | 3-line delegator + session append (see phase 1 sub-step B) |
| `set_thinking` | 3-line delegator |
| `compaction_strategy` | Keep — it genuinely needs `config`, `current_model`, `provider_registry`, `request_options_resolver` |
| `emit_compaction_end` | Keep — event emission coordinator |
| `maybe_auto_compact`, `force_compact` | Delegate to `compaction_strategy` + session, no change |
| `new_session`, `switch_session`, `fork_session` | 2-line delegator to `SessionHandle` |
| `finish_run` | Keep — session append |
| `schedule_transient_retry_with_delay`, `retry_after_overflow` | Event emission coordinators from fix 03b |
| `session_diff` | 1-line delegate to `SessionHandle::diff()` |
| `build_agent` | Keep — composes handles into `AgentLoop` |
| `session_context`, `context_without_entry`, `estimated_context_tokens` | 1-line delegates |
| `status_event` | Read-only, composes a status `AgentEvent` |
| `list_sessions` | 1-line delegate |
| `refresh_system_prompt_if_needed` | 1-line delegate to `SystemPromptCache` |

### Sub-step B — Inline one-caller helpers

If any `ControllerState` method has exactly one caller in
`InteractiveController`, evaluate whether inlining improves
clarity:

- `session_diff()` has one caller; inlining loses little and
  removes a hop. Consider.
- `list_sessions()` has one caller but the delegation through
  `SessionHandle` is useful naming. Keep.

Decide per-method. Don't inline aggressively — the goal is clarity,
not minimum method count.

### Sub-step C — Move `build_agent` to a free function or module

`build_agent` constructs an `AgentLoop` from several handles. It's
the one method on `ControllerState` that takes many inputs and
produces an output with no state mutation. Consider moving it to:

```rust
// crates/anie-cli/src/runtime/agent_builder.rs (or back into
// controller.rs as a free function)
pub(crate) fn build_agent(
    state: &ControllerState,
    cancel: CancellationToken,
) -> AgentLoop { ... }
```

This isn't strictly necessary to hit the LOC target, but it reduces
`impl ControllerState` to state-owning methods only.

### Test plan

| # | Test |
|---|------|
| 1 | Existing controller tests pass |
| 2 | `cargo clippy --workspace --all-targets -- -D warnings` passes |

### Exit criteria

- [ ] No method on `ControllerState` exceeds ~20 lines.
- [ ] Every method is either a delegator, an event coordinator, or
      a composition helper (e.g., `compaction_strategy`).
- [ ] `build_agent` is either inlined at its single caller or
      moved to a free function.

---

## Phase 3 — Extract tests off `controller.rs`

**Goal:** The tests at the bottom of `controller.rs` (lines
1575–end; ~200 LOC) move to a colocated `tests.rs` sibling or to
the handles they actually exercise.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/controller_tests.rs` | New — tests that drive `InteractiveController` / `ControllerState` end-to-end |
| `crates/anie-cli/src/controller.rs` | Remove the moved tests; `mod controller_tests;` at the end |
| `crates/anie-cli/src/runtime/config_state.rs` | Absorb any tests that only touch `ConfigState` |
| `crates/anie-cli/src/runtime/session_handle.rs` | Absorb any tests that only touch `SessionHandle` |
| `crates/anie-cli/src/retry_policy.rs` | Absorb retry-related tests (`retry_delay_prefers_retry_after_header`, `retry_delay_uses_exponential_backoff`) |

(5 files; at the cap.)

### Sub-step A — Classify the existing tests

Grep current tests in `controller.rs`:

| Test | Target |
|---|---|
| `no_tools_flag_builds_empty_registry` | `controller_tests.rs` (touches `build_tool_registry`) |
| `tool_registry_contains_core_tools_by_default` | `controller_tests.rs` |
| `dedupe_models_keeps_later_entries_for_same_provider_and_id` | Already duplicative with `model_catalog.rs` tests? If so, delete; otherwise move to `model_catalog.rs` |
| `resolve_model_honors_provider_and_id` | `model_catalog.rs` |
| `resolve_model_prefers_local_when_no_hints` | `model_catalog.rs` |
| `resolve_initial_selection_prefers_provider_only_override` | `model_catalog.rs` |
| `retry_delay_prefers_retry_after_header` | `retry_policy.rs` |
| `retry_delay_uses_exponential_backoff` | `retry_policy.rs` |
| `context_files_stamp_detects_deleted_non_newest_file` | `runtime/prompt_cache.rs` |
| `parse_thinking_accepts_supported_levels` | Wherever `parse_thinking_level` lives (likely keep if it stays a free fn in `controller.rs`; otherwise migrate) |

### Sub-step B — Move each test

For each test in the table, cut from `controller.rs` and paste
into the target module's existing `#[cfg(test)] mod tests`.
Adjust imports.

### Sub-step C — Delete duplicates

If any moved test duplicates one in the target module, delete the
duplicate.

### Test plan

| # | Test |
|---|------|
| 1 | `cargo test --workspace` passes with the same net test count (or larger if duplicates were absent) |
| 2 | Clippy clean |

### Exit criteria

- [ ] `controller.rs`'s `#[cfg(test)] mod tests` block is empty or
      limited to end-to-end controller tests.
- [ ] `retry_policy.rs` contains retry-related tests.
- [ ] `model_catalog.rs` contains model-resolution tests.
- [ ] `config_state.rs`, `session_handle.rs`, `prompt_cache.rs`
      contain handle-specific tests.

---

## Phase 4 — Verify the LOC target

**Goal:** `controller.rs` under 1000 LOC. If not, iterate until it
is.

### Sub-step A — Count

```bash
wc -l crates/anie-cli/src/controller.rs
```

Expected after phases 1–3: somewhere in 900–1100. Phase 4 exists
to close any remaining gap.

### Sub-step B — If still over 1000

Look for remaining long functions. Candidates:

- `run_print_mode` (~140 LOC today) — consider extracting the
  per-turn retry loop into a helper.
- `run_rpc_mode` — similar.
- `rpc_event_printer` + RPC serialization code (~300 LOC near end
  of file) — move to `crates/anie-cli/src/rpc.rs`.
- `From<AgentEvent> for RpcEvent` impl (~145 LOC) — moves with
  the RPC code above.

### Sub-step C — One more extraction if needed

If RPC support is ~300 LOC, extracting it hits the target on its
own. Candidate plan:

```
crates/anie-cli/src/rpc.rs
- RpcCommand + RpcEvent definitions
- From<AgentEvent> for RpcEvent impl
- rpc_event_printer function
- write_rpc_error
```

`controller.rs::run_rpc_mode` stays but calls into the `rpc`
module.

### Test plan

| # | Test |
|---|------|
| 1 | `wc -l controller.rs` ≤ 1000 |
| 2 | All tests pass |
| 3 | Clippy clean |
| 4 | Manual RPC smoke test if the RPC module was extracted: `anie --rpc` receives commands and emits events as before |

### Exit criteria

- [ ] `controller.rs` ≤ 1000 LOC.
- [ ] All behaviors identical to pre-refactor.

---

## Phase 5 — Update `ControllerState` struct layout doc

**Goal:** A new contributor reading `controller.rs` sees the final
shape and understands what each handle owns.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/controller.rs` | Add a struct-level doc comment on `ControllerState` |

### Sub-step A — Doc comment

```rust
/// Shared state for the interactive controller.
///
/// Composed of focused handles:
/// - `session: SessionHandle` — current session + fork/switch.
/// - `config: ConfigState` — AnieConfig + RuntimeState + current
///   model/thinking selections.
/// - `model_catalog: Vec<Model>` — known models across providers.
/// - `provider_registry: Arc<ProviderRegistry>` — active providers.
/// - `tool_registry: Arc<ToolRegistry>` — active tools (built once
///   at startup).
/// - `request_options_resolver: Arc<dyn RequestOptionsResolver>` —
///   auth/options per request.
/// - `prompt_cache: SystemPromptCache` — system prompt +
///   AGENTS.md context files cache.
/// - `retry_config: RetryConfig` — retry knobs.
/// - `command_registry: CommandRegistry` — builtin + future
///   extension slash commands (see `commands.rs`).
///
/// Methods on this struct are either delegators to one of the
/// handles or event-emission coordinators. Any method longer than
/// ~20 lines is a smell.
struct ControllerState { ... }
```

### Exit criteria

- [ ] Doc comment matches the actual fields.

---

## Files that must NOT change

- `crates/anie-agent/*`, `crates/anie-provider/*`,
  `crates/anie-protocol/*`, `crates/anie-session/*` —
  the controller is the seam; its dependencies do not.
- `crates/anie-tui/*` — UI-side channels are unchanged.
- `crates/anie-cli/src/commands.rs`, `compaction.rs`,
  `model_catalog.rs`, `runtime/session_handle.rs`,
  `runtime/prompt_cache.rs` — they already have their final shape.

## Dependency graph

```
Fix 03b (RetryPolicy::decide, remove is_retryable)
  └── Phase 1 (extract ConfigState)
        └── Phase 2 (slim methods)
              └── Phase 3 (move tests out)
                    └── Phase 4 (hit LOC target)
                          └── Phase 5 (doc comment)
```

Strictly sequential. Fix 03b is a prerequisite because phase 2's
delegator shape is cleaner once the retry decision logic has
moved.

## Out of scope

- Replacing `Arc<ProviderRegistry>` with something smaller. The
  `Arc` is fine; the registry composition is plan 04 territory.
- Changing how `RuntimeState` persists (file format, location).
  `ConfigState` preserves current behavior; changing it is a
  separate plan.
- Merging `model_catalog` into `ConfigState`. Revisit only if
  future work naturally pulls it in.
- Hot-reloading AGENTS.md (`docs/ideas.md` — "AGENTS.md Context
  File Handling"). `ConfigState::reload_from_disk` is the right
  seam when that lands, but the feature itself is out of scope.
- Changing the `AgentLoopConfig` shape in `anie-agent`.
