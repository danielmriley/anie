# Plan 03 — Controller decomposition

> **Revised 2026-04-17.** Phase 3's `SlashCommand` trait and
> `CommandRegistry` are updated to carry `SlashCommandSource`
> tagging (builtin / extension / prompt / skill), matching
> pi-mono's `slash-commands.ts`. This prepares the registry for
> extension-registered commands (plan 10) and future prompt-template
> and skill integrations (`docs/ideas.md`) without a second
> migration. See `pi_mono_comparison.md` for pi's shape.

> **Status (2026-04-17):** partially landed on `refactor_branch`.
> - **Phase 1 (ModelCatalog):** `019c976`. Moved to a new
>   `model_catalog.rs` as free functions (not a wrapper struct)
>   — `Vec<Model>` stays on `ControllerState`, and converting
>   to a struct would ripple through more call sites than the
>   scope allows. Module doc-comment marks the struct wrapper
>   as a future step.
> - **Phase 4 (RetryPolicy):** `9d9c236`. `RetryConfig` and
>   `retry_delay_ms` moved to `retry_policy.rs`. The *decision*
>   logic (schedule_transient_retry, retry_after_overflow,
>   should_retry_transient) remains in the controller event loop
>   — it's interleaved with event emission.
> - **Phases 2, 3, 5 pending.** Phase 2 (compaction into
>   anie-session behind a `MessageSummarizer` trait) and Phase 3
>   (slash-command registry) are substantial architectural moves
>   that need a focused session. Phase 5 (final recomposition of
>   `ControllerState`) depends on both.

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

## Phase 3 — Slash-command registry (with source tagging)

**Goal:** Replace the `handle_action` flat match with a registry-based
dispatch. `/help` derives from the registry. Each command carries
a `SlashCommandSource` tag so extensions, prompts, and skills can
register into the same registry later (plans 10 and future work)
without a second migration.

pi-mono uses the same shape: see
`~/Projects/agents/pi/packages/coding-agent/src/core/slash-commands.ts`,
which declares:

```ts
export type SlashCommandSource = "extension" | "prompt" | "skill";
```

and pi's `SlashCommandInfo` carries source + `sourceInfo`. We
adopt the same concept, extended with `Builtin` so the registry
is uniform.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/commands/mod.rs` | New — `trait SlashCommand`, `struct CommandRegistry`, `enum SlashCommandSource`, `struct SourceInfo` |
| `crates/anie-cli/src/commands/builtin.rs` | New — handler impls for `/model`, `/thinking`, `/compact`, `/fork`, `/diff`, `/session`, `/tools`, `/onboard`, `/providers`, `/clear`, `/help`, `/quit` |
| `crates/anie-cli/src/controller.rs` | Replace `handle_action` slash-command match arms with `registry.dispatch(name, args, ctx)`; keep non-slash `UiAction` variants inline |
| `crates/anie-cli/src/lib.rs` | `pub mod commands;` |

(4 files — within the 5-file cap. Splitting `commands/mod.rs` from
`commands/builtin.rs` keeps the trait + registry separate from
the long list of builtin handlers, which matches pi's split.)

### Sub-step A — Source tag type

```rust
/// Where a slash command came from.
///
/// Mirrors pi-mono's `SlashCommandSource` with an added `Builtin`
/// variant so the registry can represent every command uniformly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommandSource {
    /// Shipped with anie.
    Builtin,
    /// Registered by an extension (see plan 10).
    Extension { extension_name: String },
    /// Registered by a prompt template (future — `docs/ideas.md`).
    Prompt { template_path: PathBuf },
    /// Registered by a skill (future — `docs/ideas.md`).
    Skill { skill_name: String },
}

/// Display-facing source metadata. Used by `/help` output and by
/// future `/settings` or autocomplete views to explain where a
/// command comes from.
#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub source: SlashCommandSource,
    /// Human-readable origin (e.g., "builtin", "my-extension",
    /// "prompt: review.md").
    pub label: String,
}

impl SourceInfo {
    pub fn builtin() -> Self { ... }
    pub fn from_extension(name: &str) -> Self { ... }
    pub fn from_prompt(path: &Path) -> Self { ... }
    pub fn from_skill(name: &str) -> Self { ... }
}
```

### Sub-step B — Design the trait

```rust
pub struct CommandContext<'a> {
    pub state: &'a mut ControllerState,
    pub event_tx: &'a mpsc::Sender<AgentEvent>,
    pub ui_tx: &'a mpsc::Sender<UiEvent>,
}

#[async_trait::async_trait]
pub trait SlashCommand: Send + Sync {
    /// Command name without the leading slash (e.g., "model").
    fn name(&self) -> &str;

    /// One-line summary for `/help` output.
    fn summary(&self) -> &str;

    /// Optional longer usage string for `/help <name>`.
    fn usage(&self) -> &str { "" }

    /// Where this command came from. Defaults to builtin.
    fn source_info(&self) -> SourceInfo {
        SourceInfo::builtin()
    }

    /// Execute the command.
    async fn dispatch(
        &self,
        args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<Option<UiAction>>;
}
```

Builtins return `SourceInfo::builtin()` by default. Extensions,
prompts, and skills override `source_info()` to describe their
origin.

### Sub-step C — Registry

```rust
pub struct CommandRegistry {
    commands: HashMap<String, Arc<dyn SlashCommand>>,
}

impl CommandRegistry {
    pub fn new() -> Self;
    pub fn with_builtins() -> Self;

    /// Register a command. Returns `Err` if the name is already
    /// taken (policy: first-wins, extension attempts to override
    /// builtin get a warning and are ignored; pi does the same —
    /// see `pi_mono_comparison.md`).
    pub fn register(&mut self, cmd: Arc<dyn SlashCommand>) -> Result<(), RegisterError>;

    /// Look up a command by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn SlashCommand>>;

    /// All registered commands, in name order, with source info.
    /// Used to derive `/help` and future autocomplete.
    pub fn all(&self) -> Vec<CommandInfo>;

    pub async fn dispatch(
        &self,
        name: &str,
        args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<Option<UiAction>>;
}

pub struct CommandInfo {
    pub name: String,
    pub summary: String,
    pub source: SourceInfo,
}
```

### Sub-step D — Migrate one command at a time

Port commands one by one, starting with the simplest (`/help`,
`/quit`, `/clear`), to make each PR small. After each migration,
delete the old match arm. After all commands are migrated, delete
the giant match.

Each builtin looks like:

```rust
pub struct HelpCommand;

#[async_trait::async_trait]
impl SlashCommand for HelpCommand {
    fn name(&self) -> &str { "help" }
    fn summary(&self) -> &str { "Show available commands" }

    async fn dispatch(
        &self,
        _args: &str,
        ctx: &mut CommandContext<'_>,
    ) -> Result<Option<UiAction>> {
        let infos = ctx.state.commands.all();
        let output = format_help(&infos); // groups by source
        // emit status event with output
        Ok(None)
    }
}
```

### Sub-step E — `/help` is source-aware

`/help` groups output by `SourceInfo::source`:

```
Commands:
  Builtin:
    /model [query]       — Select model
    /thinking <level>    — Set reasoning level
    ...

  Extensions:
    /mycmd               — (from my-extension)
    ...

  Prompts:
    /review-pr           — (from prompt: review.md)
    ...
```

If the only registered commands are builtins (current state), the
groups degrade cleanly to a flat list.

### Sub-step F — Name collision policy

Matching pi: if an extension tries to register a command name that
already exists, **the first registration wins** and a warning is
logged. Builtins register first, so they always win collisions.
Prompts and skills register after extensions by convention.

```rust
pub enum RegisterError {
    DuplicateName { name: String, existing_source: SourceInfo },
}
```

### Files that must NOT change

- `crates/anie-tui/src/input.rs` — the input editor still produces
  `UiAction`s.
- `crates/anie-agent/*` — unrelated.

### Test plan

| # | Test |
|---|------|
| 1 | `registry_lookup_by_name_returns_handler` |
| 2 | `unknown_command_returns_helpful_error` |
| 3 | `help_lists_all_registered_commands_grouped_by_source` |
| 4 | `duplicate_registration_returns_err_and_keeps_first` |
| 5 | `builtin_source_info_returns_builtin_variant` |
| 6 | `extension_sourced_command_round_trips_source_info` (register a mock extension-sourced `SlashCommand`; assert `all()` reports `SlashCommandSource::Extension { .. }`) |
| 7 | Each migrated builtin: one happy-path test (e.g., `/thinking medium` mutates controller state as expected) |
| 8 | Existing TUI tests for `/help`, `/model`, `/thinking` still pass |

### Exit criteria

- [ ] All slash commands live in `commands/builtin.rs` or in small
      files imported from it.
- [ ] `handle_action` contains no slash-command match arms.
- [ ] `/help` output is derived from `registry.all()`, grouped by
      source.
- [ ] Adding a new `/settings` or `/copy` command from
      `docs/ideas.md` is: write a `SlashCommand` impl, register in
      `CommandRegistry::with_builtins()`.
- [ ] Extensions (plan 10 phase 4) can register commands with a
      non-`Builtin` source; registry accepts and exposes them.
- [ ] Prompts and skills (future) can register via the same API
      with their own `SourceInfo`.

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
