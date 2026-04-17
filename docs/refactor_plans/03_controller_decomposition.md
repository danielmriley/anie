# Plan 03 — Controller decomposition

## Motivation

`crates/anie-cli/src/controller.rs` is 1967 LOC. `ControllerState`
(lines 667–1106) is a God object owning:

- Session management (entries, fork, switch, diff).
- Model catalog (resolution, caching, upsert).
- Compaction logic (`maybe_auto_compact`, `force_compact`,
  `retry_after_overflow` — ~80% duplicated code across the three).
- Tool registry construction (rebuilt per run; lines 969–984).
- System-prompt assembly.
- Runtime-state persistence.
- Auth resolution hookup.
- Retry bookkeeping.

Additional concerns:

- Model resolution is spread across four functions at lines 1170,
  1183, 1260, 1286, 1329.
- The slash-command dispatcher is a flat 20-arm match in
  `handle_action` (inside `impl InteractiveController`, roughly lines
  426–591). Help text is maintained separately from the handlers.
- `anie-session` receives `ProviderRegistry` for compaction —
  `auto_compact` / `force_compact` call sites leak provider types
  into a crate that should only know about persistence.
- Print, RPC, and interactive modes share `ControllerState` but
  exercise it through different entry points; their state machines
  are entangled.

## Design principles

1. **Split by reason-to-change, not by word-count.** Each new type
   should own one concern, not "a chunk of what used to be
   `ControllerState`."
2. **`anie-session` stops knowing about providers.** Compaction
   takes a summarization callback, not a `ProviderRegistry`.
3. **Slash commands are a registry.** One source of truth for
   `/help`, dispatch, and autocomplete.
4. **No behavior change.** Every keystroke produces the same
   outcome. All tests still pass.
5. **Phase boundaries are reviewable.** ≤5 files per phase; each
   phase leaves the build green.

## Current file layout (verified 2026-04-17)

| Lines | Contents |
|---|---|
| 1–45 | Imports |
| 47–67 | `RetryConfig` |
| 68–104 | `run_interactive_mode` |
| 105–244 | `run_print_mode` |
| 245–307 | `run_rpc_mode` |
| 308–666 | `InteractiveController` (struct, `CurrentRun`, huge impl) |
| 667–1106 | `ControllerState` (struct + impl) |
| 1107–1169 | `prepare_controller_state` |
| 1170–1381 | Model resolution (5 functions) |
| 1382–1402 | `build_tool_registry` |
| 1403–1435 | `build_system_prompt` |
| 1436–1490 | `rpc_event_printer`, `write_rpc_error`, signal forwarder |
| 1491–1644 | UI formatting helpers (`format_thinking`, `format_tokens`, etc.) |
| 1646–1790 | `RpcCommand`, `RpcEvent`, `From<AgentEvent>` |
| 1791–end | Tests |

---

## Phase 1 — Extract `ModelCatalog`

**Goal:** Move the five model-resolution functions and the upsert
logic out of `controller.rs` into a dedicated type.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/model_catalog.rs` | New — `struct ModelCatalog`, moves of `build_model_catalog`, `resolve_initial_selection`, `resolve_requested_model`, `resolve_model`, `fallback_model_from_provider`, `upsert_model`, `dedupe_models` |
| `crates/anie-cli/src/lib.rs` | `pub mod model_catalog;` |
| `crates/anie-cli/src/controller.rs` | Delete moved code; update call sites to use `ModelCatalog` methods |

### Sub-step A — Design the type

```rust
pub struct ModelCatalog {
    models: Vec<Model>,
    default_provider_hint: Option<String>,
}

impl ModelCatalog {
    pub async fn load(config: &AnieConfig) -> (Self, bool);
    pub fn resolve_initial_selection(&self, ...) -> InitialSelection;
    pub fn resolve_requested_model(&self, ...) -> Option<Model>;
    pub fn resolve(&self, provider: &str, id: &str) -> Option<Model>;
    pub fn upsert(&mut self, model: &Model);
    pub fn fallback_for_provider(&self, provider: &str) -> Option<Model>;
}
```

### Sub-step B — Tests

| # | Test |
|---|------|
| 1 | `resolve_by_provider_and_id_hits_existing` |
| 2 | `resolve_requested_model_with_only_id_disambiguates_unique` |
| 3 | `resolve_requested_model_with_ambiguous_id_returns_none` |
| 4 | `upsert_replaces_matching_provider_id_pair` |
| 5 | `fallback_for_provider_returns_first_registered_model` |
| 6 | `dedupe_models_preserves_order_and_drops_repeats` |

Place tests in `model_catalog.rs` as `#[cfg(test)] mod tests`.

### Exit criteria

- [ ] `controller.rs` no longer contains model resolution helpers.
- [ ] `ModelCatalog` has unit tests.
- [ ] All existing tests still pass.

---

## Phase 2 — Move compaction into `anie-session` behind a callback

**Goal:** `ControllerState::maybe_auto_compact`,
`ControllerState::force_compact`, and the overflow-retry path stop
duplicating compaction config building. The session crate owns
compaction; it takes a "summarize these messages" callback instead
of a `ProviderRegistry`.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-session/src/lib.rs` | Change `auto_compact` / `force_compact` signatures to take a `F: Fn(Vec<Message>) -> BoxFuture<'_, Result<Compaction, ProviderError>>` or a `trait Summarizer` |
| `crates/anie-cli/src/compaction_strategy.rs` | New — `struct CompactionStrategy` builds the summarizer closure from `ProviderRegistry` + config |
| `crates/anie-cli/src/controller.rs` | Delete the three duplicated compaction bodies; call `compaction_strategy.run(...)` in all three call sites |
| `crates/anie-cli/src/lib.rs` | `pub mod compaction_strategy;` |

### Sub-step A — Define the summarizer boundary

In `anie-session`, introduce:

```rust
#[async_trait::async_trait]
pub trait MessageSummarizer: Send + Sync {
    async fn summarize(
        &self,
        messages: &[Message],
        reason: CompactionReason,
    ) -> Result<Compaction, SessionError>;
}
```

(`Compaction` and `CompactionReason` already exist; if not, put the
minimum struct the session needs.)

`auto_compact` and `force_compact` become:

```rust
pub async fn auto_compact(
    &mut self,
    summarizer: &dyn MessageSummarizer,
    config: &CompactionConfig,
) -> Result<CompactionOutcome, SessionError>;
```

No provider types appear in the signature.

### Sub-step B — Build the summarizer in `anie-cli`

`CompactionStrategy` wraps `ProviderRegistry` + current `Model` +
`StreamOptions` and implements `MessageSummarizer`.

### Sub-step C — Collapse the three call sites

`maybe_auto_compact`, `force_compact`, and `retry_after_overflow`
become thin wrappers that:

1. Build the `CompactionConfig`.
2. Call `session.auto_compact(&strategy, &cfg)` (or `force_compact`).
3. Translate the outcome into the appropriate event stream.

All three should share a single helper (e.g.,
`async fn run_compaction(&mut self, reason: CompactionReason)`).

### Files that must NOT change

- `crates/anie-protocol/*` — the `Message` type stays put.
- `crates/anie-provider/*` — the `Provider` trait is untouched.

### Test plan

| # | Test |
|---|------|
| 1 | `session_auto_compact_calls_summarizer_with_expected_messages` (session-level, with mock summarizer) |
| 2 | `session_force_compact_calls_summarizer` |
| 3 | `compaction_strategy_produces_compaction_entry` (cli-level, with mock provider) |
| 4 | `overflow_retry_invokes_same_strategy` |
| 5 | Existing integration tests pass. |

### Exit criteria

- [ ] `anie-session` has no reference to `anie-provider`-specific
      types (beyond what it already used).
- [ ] Compaction bodies in the three call sites are ≤5 lines each.
- [ ] Mock-based session tests for compaction exist.

---

## Phase 3 — Slash-command registry

**Goal:** Replace the `handle_action` flat match with a registry-based
dispatch. `/help` derives from the registry.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/commands.rs` | New — `trait SlashCommand`, `struct CommandRegistry`, handler impls for `/model`, `/thinking`, `/compact`, `/fork`, `/diff`, `/session`, `/tools`, `/onboard`, `/providers`, `/clear`, `/help`, `/quit` |
| `crates/anie-cli/src/controller.rs` | Replace `handle_action` slash-command match arms with `registry.dispatch(name, args, ctx)`; keep non-slash `UiAction` variants inline |
| `crates/anie-cli/src/lib.rs` | `pub mod commands;` |

(3 files — well under the 5-file cap.)

### Sub-step A — Design the trait

```rust
pub struct CommandContext<'a> {
    pub state: &'a mut ControllerState,
    pub event_tx: &'a mpsc::Sender<AgentEvent>,
    pub ui_tx: &'a mpsc::Sender<UiEvent>,
}

pub trait SlashCommand: Send + Sync {
    fn name(&self) -> &'static str;
    fn summary(&self) -> &'static str;
    fn usage(&self) -> &'static str { "" }
    fn dispatch(&self, args: &str, ctx: &mut CommandContext<'_>) -> Result<Option<UiAction>>;
}
```

### Sub-step B — Migrate one command at a time

Port commands one by one, starting with the simplest (`/help`,
`/quit`, `/clear`), to make each PR small. After each migration,
delete the old match arm. After all commands are migrated, delete
the giant match.

### Sub-step C — Derive `/help` from the registry

`/help`'s handler returns a formatted list by iterating
`registry.commands().map(|c| (c.name(), c.summary()))`.

### Files that must NOT change

- `crates/anie-tui/src/input.rs` — the input editor still produces
  `UiAction`s.
- `crates/anie-agent/*` — unrelated.

### Test plan

| # | Test |
|---|------|
| 1 | `registry_lookup_by_name_returns_handler` |
| 2 | `unknown_command_returns_helpful_error` |
| 3 | `help_lists_all_registered_commands` |
| 4 | Each migrated command: one happy-path test (e.g., `/thinking medium` mutates controller state as expected). |
| 5 | Existing TUI tests for `/help`, `/model`, `/thinking` still pass. |

### Exit criteria

- [ ] All slash commands live in `commands.rs` or in small files
      imported from it.
- [ ] `handle_action` contains no slash-command match arms.
- [ ] `/help` output is derived, not hand-maintained.
- [ ] New `/settings` or `/copy` commands from `docs/ideas.md` can
      be added by writing a single handler impl.

---

## Phase 4 — Extract `RetryPolicy` and simplify the event loop

**Goal:** The print-mode event loop (lines 105–244) has retry
decisions, continuation decisions, and overflow-retry decisions all
interleaved. Extract a `RetryPolicy` type that owns that decision
tree.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/retry_policy.rs` | New — `struct RetryPolicy`, decision functions |
| `crates/anie-cli/src/controller.rs` | Replace inline retry-decision code with `retry_policy.decide(outcome)` |
| `crates/anie-cli/src/lib.rs` | `pub mod retry_policy;` |

### Sub-step A — Enumerate outcomes

From the existing code, retry decisions depend on:

- `ProviderError` variant (Auth, RateLimited, ContextOverflow,
  Http(status), Stream, Other).
- Whether a compaction has already been attempted this run.
- Retry attempt count vs. `RetryConfig` limits.

Encode as:

```rust
pub enum RetryDecision {
    Retry { delay_ms: u64 },
    Compact,
    Giveup(String),
}

impl RetryPolicy {
    pub fn decide(
        &self,
        error: &ProviderError,
        attempt: u32,
        already_compacted: bool,
    ) -> RetryDecision;
}
```

### Sub-step B — Migrate call sites

Replace the interleaved `if let ProviderError::ContextOverflow ...`
/ `retry_delay_ms(...)` blocks in the event loop with a single
`match retry_policy.decide(...)`.

### Sub-step C — Tests

| # | Test |
|---|------|
| 1 | `auth_error_gives_up_immediately` |
| 2 | `rate_limit_returns_retry_with_backoff` |
| 3 | `context_overflow_triggers_compact_on_first_attempt` |
| 4 | `context_overflow_gives_up_if_already_compacted` |
| 5 | `http_5xx_retries_up_to_limit` |
| 6 | `http_4xx_gives_up` |
| 7 | `stream_error_retries_limited_times` |

### Exit criteria

- [ ] Retry logic lives in `retry_policy.rs` and is pure (no I/O).
- [ ] Event loops call one method per decision point.
- [ ] All cases above are unit-tested.

---

## Phase 5 — Split `ControllerState` into focused types

**Goal:** The god object is gone. Move what's left into three types
with clear ownership.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/runtime/session_handle.rs` | New — owns session, fork, switch, diff |
| `crates/anie-cli/src/runtime/config_state.rs` | New — owns `AnieConfig`, `RuntimeState`, `system_prompt` refresh |
| `crates/anie-cli/src/runtime/mod.rs` | Re-exports |
| `crates/anie-cli/src/controller.rs` | `ControllerState` becomes a composition of `ModelCatalog`, `SessionHandle`, `ConfigState`, `CompactionStrategy`, `RetryPolicy` |

(4 files, within cap.)

### Sub-step A — Extract `SessionHandle`

Owns the currently-open session, fork operations, diff rendering,
session-list queries. Exposes methods; holds no config or model data.

### Sub-step B — Extract `ConfigState`

Owns the `AnieConfig`, the current `Model` choice, the cached
`system_prompt`. Knows how to refresh the system prompt from project
context files (see `docs/ideas.md` — "AGENTS.md Context File
Handling"; refreshing mid-session is tracked there, not here).

### Sub-step C — Recompose `ControllerState`

After the extractions, `ControllerState` should be a struct of
handles:

```rust
pub struct ControllerState {
    catalog: ModelCatalog,
    session: SessionHandle,
    config: ConfigState,
    compaction: CompactionStrategy,
    retry: RetryPolicy,
    tools: Arc<ToolRegistry>,
    auth: AuthResolver,
}
```

Methods on `ControllerState` should be thin coordinators — delegate
to the handles, don't reimplement their logic.

### Sub-step D — Cache the `ToolRegistry`

`build_tool_registry` currently runs per agent run (lines 969–984).
Move it to `ControllerState` construction and share via `Arc`.

### Test plan

| # | Test |
|---|------|
| 1 | `controller_state_construction_builds_tool_registry_once` |
| 2 | `session_handle_fork_creates_new_session_with_parent_link` |
| 3 | `config_state_refresh_reloads_system_prompt_from_disk` |
| 4 | All existing print/interactive/rpc tests pass unchanged. |

### Files that must NOT change

- `crates/anie-agent/*` — `AgentLoop` config interface stays stable.
- `crates/anie-protocol/*`, `crates/anie-provider/*` — unchanged.
- `crates/anie-tui/*` — the TUI already talks via the `UiAction` /
  `AgentEvent` channels.

### Exit criteria

- [ ] `ControllerState` has zero "doing logic" methods longer than
      ~20 lines.
- [ ] Each extracted type lives in its own file.
- [ ] `ToolRegistry` is built once per `ControllerState`.
- [ ] `controller.rs` is under 1000 LOC.

---

## Files that must NOT change in any phase

- `crates/anie-protocol/*`
- `crates/anie-provider/*` — the trait is a contract, not touchable
  by this plan.
- `crates/anie-agent/*` — the agent loop's public API is the
  stability point.
- `crates/anie-tui/*` — the TUI talks via action enums and events;
  neither shape changes in this plan.

## Dependency graph

```
Phase 1 (ModelCatalog) ──┐
Phase 2 (Compaction)   ──┼──► Phase 5 (final compose)
Phase 3 (Commands)     ──┤
Phase 4 (RetryPolicy)  ──┘
```

Phases 1–4 are independent. Land them in any order. Phase 5 depends
on all of them.

## Out of scope

- OAuth / subscription auth — tracked in `docs/ideas.md`.
- Re-reading AGENTS.md mid-session — tracked in `docs/ideas.md`.
- New slash commands (`/settings`, `/copy`, `/resume`, `/new`, etc.)
  — these become cheap after phase 3, but adding them is feature
  work, not cleanup.
- Sandboxing tool execution.
