# `/context-length` slash command for user-side `num_ctx` override

**Add a first-class slash command that lets the user override
the `num_ctx` value sent to Ollama's `/api/chat`, with
per-model scope, persisted across sessions, and visible
feedback about the effective value and its source. Depends on
[`../ollama_native_chat_api/README.md`](../ollama_native_chat_api/README.md).**

## Context

This is the second deferred item in
[`docs/ollama_capability_discovery/README.md:880-…`](../ollama_capability_discovery/README.md).
Its implementation only becomes meaningful once the native
`/api/chat` codepath is shipped — the OpenAI-compat layer
silently drops `num_ctx`, so an override has nowhere to land
without Plan 1 first.

### Why a slash command

After Plan 1, anie automatically sets
`options.num_ctx = Model.context_window` on every request, and
`Model.context_window` for Ollama comes from `/api/show.model_info
["{arch}.context_length"]`. In the qwen3.5 case that's 262 144.
Correct. But:

1. **VRAM budget.** The user may not have enough memory to
   load 262 k of context. The model loads with the requested
   `num_ctx`, and if it OOMs Ollama returns a clear error —
   but the user has no knob to dial it down without editing
   the catalog.
2. **Latency / KV-cache size.** Larger `num_ctx` means Ollama
   allocates a bigger KV cache; perceived latency on first
   token grows. The user may prefer a smaller window
   intentionally.
3. **Experimentation.** "What if I bumped qwen3.5 to 128 k"
   or "does this model really work at 512 k?" — no setup
   friction should stand between the user and trying.

Hand-editing `.anie/config.toml` to change `context_window` on
a single model is awkward and doesn't survive a re-discovery.
A slash command that persists per-model and re-applies across
sessions is the right surface.

## Requirements

- `/context-length` (no args) — report the current model's
  **effective** `num_ctx`. If a runtime override is active,
  show both the override and the baseline
  (`Model.context_window`) so the user can see what `reset`
  would restore.
- `/context-length <N>` — set an override for the current
  model. `N` is a positive integer in a reasonable range
  (2 048 ≤ N ≤ 1 048 576). Applied on the next request.
- `/context-length reset` — clear the override for the current
  model. Falls back to `Model.context_window`.
- Models whose `Model.api` is not `ApiKind::OllamaChatApi`
  reject the command with a friendly message:
  "`/context-length` only applies to Ollama native /api/chat
  models — selected model '{provider}:{id}' uses {api}."
- Persisted per-model across sessions.
- The existing status-bar context denominator uses the
  effective value, so progress / compaction pressure matches
  the next request. A source marker like `(override)` is
  deferred UI polish.

## Design

### Storage

Per-model `num_ctx` overrides live in the runtime state file
`~/.anie/state.json` at
[`runtime_state.rs`](../../crates/anie-cli/src/runtime_state.rs),
next to the existing `provider` / `model` / `thinking` fields.
New field:

```rust
pub struct RuntimeState {
    // …existing fields…

    /// Per-model `num_ctx` override for Ollama models. Keyed by
    /// "{provider}:{model_id}". Empty when no override is set.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub ollama_num_ctx_overrides: HashMap<String, u64>,
}
```

Stored in state.json (not config.toml) because this is a
runtime preference, not a config declaration. The runtime
override intentionally shadows config/discovery while active;
`/context-length reset` returns to the baseline
`Model.context_window`.

### Effective-value resolution

**Runtime override wins unconditionally over the baseline.** The
baseline (`Model.context_window`) already bakes in whichever
of config-pinned / discovered / fallback applies; the slash
command is a short-term adjustment on top. This is simpler than
the four-tier ladder I first sketched, and it matches user
intent: "I ran `/context-length 16 384` — honor that, even if
config.toml once pinned a different value for this model."

Two tiers, short-circuit in order:

1. **Runtime override**:
   `RuntimeState::ollama_num_ctx_overrides["{provider}:{id}"]`
   → provider receives `Some(value)`.
2. **Baseline**: no runtime override → provider receives
   `None`, falls back to `Model.context_window` (which is
   already whichever of config-pinned / discovered / 32 k
   applies, per Plan 1's catalog-load path).

Resolved in `ConfigState`, then snapshotted into each
agent-loop / compaction strategy before a request starts:

1. `ConfigState` owns private `RuntimeState`, so it exposes
   resolver methods that read `ollama_num_ctx_overrides`
   without leaking the map.
2. `build_agent(state)` in `controller.rs` snapshots
   `ConfigState::active_ollama_num_ctx_override()` into a new
   `AgentLoopConfig` field.
3. `AgentLoop::run` copies that field into
   `StreamOptions::num_ctx_override` when it assembles the
   provider request at
   [`anie-agent/src/agent_loop.rs:413-432`](../../crates/anie-agent/src/agent_loop.rs).
4. `CompactionStrategy` gets the same snapshot so summary
   requests also honor the user's `num_ctx`.
5. `OllamaChatProvider` stays stateless — no map lookups, no
   controller pointers — it just reads one `Option<u64>` and
   writes it to the body if set, otherwise uses
   `Model.context_window`.

The effective context window also drives compaction thresholds:
`ControllerState::compaction_strategy` must use
`ConfigState::effective_ollama_context_window()` rather than
raw `current_model().context_window`, otherwise a lower
override could let the transcript grow beyond the `num_ctx`
that will be sent to Ollama.

**Persistence model for a user who wants a permanent change:**
edit `~/.anie/config.toml` and set
`[[providers.ollama.models]] context_window = N` for the
model. That bakes into the catalog's `Model.context_window`,
so the baseline moves. Clear the runtime override (with
`/context-length reset`) to stop it shadowing the new
baseline. Explained in PR 3's no-arg `/context-length`
message.

### Command wiring

Slash-command metadata goes through
[`anie-cli/src/commands.rs`](../../crates/anie-cli/src/commands.rs)
— same surface the existing commands use. Dispatch lands in
[`controller.rs`](../../crates/anie-cli/src/controller.rs)
alongside `UiAction::SetModel` / `UiAction::SetThinking`. New
`UiAction` variant:

```rust
UiAction::ContextLength(Option<String>),
// None          → query
// Some("reset") → reset
// Some("<N>")   → set after controller-side validation
```

### Argument parsing

The current `ArgumentSpec::FreeForm` only validates presence;
it cannot validate `<N>|reset`. Add a dedicated static variant
in [`anie-tui::ArgumentSpec`](../../crates/anie-tui/src/commands.rs):

```rust
ArgumentSpec::ContextLengthOverride
```

- Accept `<integer>` or the literal string `reset`.
- Accept no argument for the query form.
- Reject integers < 2 048 or > 1 048 576 with a message.
- Reject non-integer / non-`reset` strings with a message
  pointing at the correct usage.

Echoed in the autocomplete-popup hint column as
`/context-length [<N>|reset]`.

### Status-bar / `/model` visibility

Required: the existing status bar's `{used}/{context_window}`
denominator must use the **effective** context window. This
falls out of `ControllerState::status_event` once it uses
`ConfigState::effective_ollama_context_window()`.

Deferred: a cosmetic source marker such as `ctx 16 384
(override)`. That requires extending
`AgentEvent::StatusUpdate`, `interactive_mode::apply_status_event`,
`StatusBarState`, and `render_status_bar`; it is not required
for correctness because `/context-length` query reports the
source explicitly.

## Files to touch

| File | PR | What |
|------|----|------|
| `crates/anie-cli/src/runtime_state.rs` | 1 | Add `ollama_num_ctx_overrides` field with forward-compat serde |
| `crates/anie-provider/src/options.rs` | 2A | Add `StreamOptions::num_ctx_override: Option<u64>` |
| `crates/anie-agent/src/agent_loop.rs` | 2A | Add `AgentLoopConfig::ollama_num_ctx_override` and copy it into `StreamOptions` |
| `crates/anie-cli/src/compaction.rs` | 2A | Keep summary requests compiling with `num_ctx_override: None`; effective override wiring lands in 2B |
| `crates/anie-providers-builtin/src/ollama_chat/convert.rs` | 2A | Prefer `options.num_ctx_override` over `model.context_window` |
| `crates/anie-cli/src/runtime/config_state.rs` | 2B | Add override getters/setters and effective context-window helpers |
| `crates/anie-cli/src/controller.rs` | 2B | Snapshot override into `AgentLoopConfig`; use effective context window for compaction + status events |
| `crates/anie-cli/src/compaction.rs` | 2B | Store/pass `num_ctx_override` in compaction summary requests |
| `crates/anie-cli/src/commands.rs` | 3 | Register `/context-length` metadata |
| `crates/anie-tui/src/commands.rs` | 3 | Add `ArgumentSpec::ContextLengthOverride` |
| `crates/anie-tui/src/app.rs` | 3 | Dispatch `/context-length` as `UiAction::ContextLength(Option<String>)` |
| `crates/anie-cli/src/controller.rs` | 3 | Handle query/set/reset; validate, persist, and emit status |

## Phased PRs

### PR 1 — Storage field + forward-compat

**Why first:** smallest, schema-only. Adds the persistence
surface. No user-visible change yet.

**Scope:**

- Add the `ollama_num_ctx_overrides` field to `RuntimeState`
  with `#[serde(default, skip_serializing_if = "HashMap::is_empty")]`.
- Forward-compat test: a state.json file written before this
  plan (no field) loads cleanly with an empty map.
- Reverse compat: state.json with an entry serialized still
  deserializes if the binary is downgraded to the pre-plan
  version (because the field is ignored by default serde).

**Tests:**

- `runtime_state_forward_compat_loads_state_without_num_ctx_overrides`
- `runtime_state_serializes_num_ctx_overrides_when_non_empty`
- `runtime_state_omits_num_ctx_overrides_field_when_empty`

### PR 2A — Provider/agent `num_ctx_override` plumbing

**Why second-A:** the original PR 2 touched six files, so it is
split to respect the five-file rule. This first slice adds the
provider-facing option and snapshots it through `AgentLoopConfig`;
there is still no user-facing command and no runtime-state lookup.

**Scope:**

- Extend `StreamOptions` in
  [`anie-provider/src/options.rs:29-40`](../../crates/anie-provider/src/options.rs):

  ```rust
  pub struct StreamOptions {
      // …existing fields…

      /// Override for Ollama's `options.num_ctx`. `None` means
      /// the provider falls back to `Model.context_window`.
      /// Populated by AgentLoop from AgentLoopConfig.
      /// Ignored by non-Ollama providers.
      pub num_ctx_override: Option<u64>,
  }
  ```

  Additive default-able field; no session-schema concern.
  Every existing `StreamOptions { ... }` literal must be
  updated, including `anie-agent/src/agent_loop.rs`,
  `anie-cli/src/compaction.rs`, provider unit tests, and
  integration-test helpers.

- Add `ollama_num_ctx_override: Option<u64>` to
  [`AgentLoopConfig`](../../crates/anie-agent/src/agent_loop.rs)
  with a builder-style setter, defaulting to `None` in
  `AgentLoopConfig::new`. `AgentLoop::run` copies it into
  `StreamOptions` at the actual construction site:

  ```rust
  let options = StreamOptions {
      // …existing fields…
      num_ctx_override: self.config.ollama_num_ctx_override,
  };
  ```

- `ollama_chat/convert.rs` body builder prefers
  `options.num_ctx_override` over `model.context_window` when
  present:

  ```rust
  let num_ctx = options.num_ctx_override.unwrap_or(model.context_window);
  body["options"] = json!({ "num_ctx": num_ctx });
  ```

- `compaction.rs` explicitly initializes the new field to `None`
  until PR 2B wires in the effective override.

- Non-Ollama providers ignore the field entirely.

**Tests:**

- `stream_options_num_ctx_override_default_is_none`
- `agent_loop_config_num_ctx_override_defaults_to_none`
- `agent_loop_copies_num_ctx_override_into_stream_options`
- `ollama_chat_body_prefers_num_ctx_override_over_context_window`
- `ollama_chat_body_uses_context_window_when_override_is_none`

### PR 2B — Runtime config/controller/compaction override wiring

**Why second-B:** once the provider-facing option exists, wire the
runtime-state resolver through CLI state, controller snapshots,
compaction thresholds, summary requests, and status updates. The
provider remains stateless and receives a single `Option<u64>` via
`StreamOptions`.

**Scope:**

- New helper in
  [`anie-cli/src/runtime/config_state.rs`](../../crates/anie-cli/src/runtime/config_state.rs)
  (co-located with the state it reads):

  ```rust
  impl ConfigState {
      pub(crate) fn active_ollama_num_ctx_override(&self) -> Option<u64> { … }
      pub(crate) fn effective_ollama_context_window(&self) -> u64 { … }
  }
  ```

  Rule: runtime override wins unconditionally.

  ```rust
  let key = format!("{}:{}", self.current_model().provider,
                    self.current_model().id);
  self.runtime_state.ollama_num_ctx_overrides.get(&key).copied()
  ```

  The helper returns `None` unless
  `self.current_model().api == ApiKind::OllamaChatApi`. Custom
  provider names backed by Ollama still work; provider-name-only
  checks are deliberately avoided.

  Config-pinned and discovered values live in
  `Model.context_window` (baked in at catalog load); the
  provider reads those when the override is `None`. No
  four-tier ladder, no second lookup into `AnieConfig`.

- `build_agent(state)` in
  [`controller.rs`](../../crates/anie-cli/src/controller.rs)
  snapshots `state.config.active_ollama_num_ctx_override()`
  into `AgentLoopConfig`. Then
  [`AgentLoop::run`](../../crates/anie-agent/src/agent_loop.rs)
  receives it through the PR 2A `AgentLoopConfig` field.

- `ControllerState::compaction_strategy` uses
  `state.config.effective_ollama_context_window()` for the
  `CompactionConfig.context_window` threshold. It also passes
  the active override into `CompactionStrategy::new`, and
  `CompactionStrategy::summarize` copies it into its
  `StreamOptions`. This keeps auto-compaction, overflow
  recovery, and summary requests consistent with the next
  Ollama `num_ctx`.

- `ControllerState::status_event` emits
  `context_window: state.config.effective_ollama_context_window()`
  so the status bar denominator reflects the active override.

**Tests:**

- `active_ollama_num_ctx_override_returns_none_when_no_runtime_entry`
- `active_ollama_num_ctx_override_returns_some_when_runtime_entry_present`
- `active_ollama_num_ctx_override_keyed_by_provider_and_model_tuple`
  (qwen3:32b on `ollama1` and qwen3:32b on `ollama2` track
  separately).
- `active_ollama_num_ctx_override_ignores_non_ollama_chat_api_model`
- `effective_ollama_context_window_uses_override_when_present`
- `compaction_strategy_uses_effective_ollama_context_window`
- `compaction_summary_request_passes_num_ctx_override`
- `build_agent_snapshots_num_ctx_override_into_agent_loop_config`

### PR 3A — `/context-length` controller action

**Why third-A:** the original PR 3 crosses six files, so it is
split to respect the five-file rule. This backend slice adds
the `UiAction`, controller validation/mutation, persistence, and
request-consistency tests without registering the slash command
yet.

**Scope:**

- Add `UiAction::ContextLength(Option<String>)`.
- Add `ConfigState` mutators that set/reset the current model's
  `{provider}:{model_id}` runtime override.
- Add a controller handler for `UiAction::ContextLength` in
  [`controller.rs:handle_action`](../../crates/anie-cli/src/controller.rs)
  (near the existing `UiAction::SetModel` /
  `UiAction::SetThinking` arms).
- Validation — reject models whose `api` is not
  `ApiKind::OllamaChatApi` with a friendly system message.
  Reject out-of-range values with a system message pointing at
  the accepted range. Accept `reset` and remove the key from
  the map.
- Runtime safety — reject set/reset while a run is active or a
  retry backoff is armed. The next retry should not silently
  change `num_ctx` relative to the failed attempt. The query
  form may still run while idle/active because it does not
  mutate state.
- Persist immediately via the existing
  `ConfigState::persist_runtime_state` path.
- After set/reset, emit `AgentEvent::StatusUpdate` so the
  status bar denominator reflects the new effective context
  window before the next request.
- `/context-length` with no args emits a system message:
  `"Current context window: 16 384 (runtime override; baseline 262 144)"`
  when an override is active, or
  `"Current context window: 262 144"` when no override is set.
  The "baseline" in the override message is
  `Model.context_window`; the user can `/context-length reset`
  to return to it. Format mirrors `/thinking`'s feedback.

**Tests:**

- `context_length_sets_override_for_current_ollama_model`
- `context_length_reset_clears_override`
- `context_length_on_non_ollama_model_emits_friendly_error`
- `context_length_rejects_out_of_range_value`
- `context_length_rejects_unparseable_argument`
- `context_length_set_rejected_while_run_active`
- `context_length_set_rejected_while_retry_pending`
- `context_length_no_args_reports_current_effective_value_and_source`
- `context_length_set_emits_status_update_with_effective_context_window`
- `context_length_override_persists_across_session_restart`
- `context_length_override_applies_to_next_request_without_reload`
  (recording-provider test: apply an override, build the next
  agent request without reloading config, and assert the
  request options carry the new `num_ctx`).

### PR 3B — `/context-length` slash-command registration

**Why third-B:** once the controller action is safe and tested,
wire the user-facing slash command and TUI-side argument
validation.

**Scope:**

- Register `/context-length` in
  [`anie-cli/src/commands.rs` `builtin_commands()`
  function](../../crates/anie-cli/src/commands.rs) — pattern
  identical to the existing `/thinking` registration.
- Argument spec in
  [`anie-tui/src/commands.rs::ArgumentSpec`](../../crates/anie-tui/src/commands.rs):
  `ArgumentSpec::ContextLengthOverride`, accepting no arg,
  `<positive-integer>`, or the literal `reset`.
- Dispatch validated `/context-length [N|reset]` to
  `UiAction::ContextLength`.

**Tests:**

- `context_length_command_registered_with_expected_arg_spec`
- `context_length_arg_spec_accepts_query_set_and_reset`
- `context_length_arg_spec_rejects_out_of_range_and_unparseable_values`
- `context_length_slash_command_dispatches_ui_action`

## Test plan

Per-PR tests above. Cross-cutting manual smoke:

| # | Test | Where |
|---|------|-------|
| Manual | `/context-length` on qwen3.5:9b after Plan 1 PR 6 — expect `Current context window: 262 144 (discovered)`. | smoke |
| Manual | `/context-length 32768`, then a prompt that does real work. Verify via `curl /api/ps` that Ollama reloaded with num_ctx=32768. | smoke |
| Manual | `/context-length reset`, another prompt. Verify `/api/ps` now shows the discovered value. | smoke |
| Manual | `/context-length 0` or `/context-length huge` — expect a rejection, no state change, no reload. | smoke |
| Manual | `/context-length` on a hosted (non-Ollama) model — expect the friendly error; state unchanged. | smoke |
| Manual | Restart anie; previously-set override still applies to the next request (persistence). | smoke |
| Manual | Status bar denominator changes to the effective value after set/reset; switch models; value updates. | smoke |
| Auto | `cargo test --workspace` green. | CI |
| Auto | `cargo clippy --workspace --all-targets -- -D warnings` clean. | CI |

## Risks

- **Model reload cost.** Every `num_ctx` change forces a
  reload (5–30 seconds, depending on model size and hardware).
  Users flipping back and forth between values will notice.
  PR 3's system-message on override change should include a
  heads-up: "Context window set to 16 384. Ollama will reload
  the model on the next request (~5–30 s for this model)."
- **Overrides drift from catalog.** A user who overrides to
  128 k and then re-runs discovery (which might not change
  `Model.context_window` in config but might refresh the
  catalog) could end up with a stale override for a model
  they're no longer using. Mitigation: overrides are keyed
  on `{provider}:{model_id}`; switching models naturally
  ignores unused keys. We do not garbage-collect orphan keys
  — the map stays small (dozens of entries at most) and
  pruning adds complexity. Punted.
- **Downgrade compatibility.** A user who sets an override on
  this version and then downgrades to a pre-plan binary
  silently loses the override behavior (the field is ignored
  by default serde). Not a regression because pre-plan also
  had no override. Documented as expected.
- **Runtime override shadows a config-pinned value.** A user
  who set `context_window` in `config.toml` years ago and then
  runs `/context-length 16384` today will see 16 384 — not the
  config value. Intentional (the slash command is the
  foreground control) but could surprise someone who forgot
  about the config. Mitigation: PR 3's no-arg
  `/context-length` output distinguishes "override" from
  "baseline" explicitly so the user can tell at a glance.
  `/context-length reset` always returns to the baseline.
- **Compaction mismatch.** If compaction keeps using
  `Model.context_window` while the provider sends a smaller
  override, anie could retain more input than Ollama accepts.
  PR 2 explicitly routes the effective context window into
  `CompactionConfig` and compaction summary `StreamOptions`.
- **Retry consistency.** Changing `num_ctx` during a pending
  retry would make the retry request differ from the failed
  request. PR 3 rejects set/reset while a retry is armed.
- **Tool autocomplete list grows.** Minor — one more entry.
  Well within the existing UI budget.

## Exit criteria

- [ ] PR 1 merged: `RuntimeState.ollama_num_ctx_overrides`
      field exists with forward-compat serde.
- [ ] PR 2 merged: effective `num_ctx` reaches
      `AgentLoop`, `CompactionStrategy`, `OllamaChatProvider`,
      compaction thresholds, and status updates.
- [ ] PR 3 merged: `/context-length` command works end-to-end
      with all four shapes (set, reset, query, error).
- [ ] Every cross-cutting smoke test above passes against a
      real Ollama instance.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] No regressions in the parent Plan 1 smoke tests.

## Deferred

- **Config-file override command.** `/context-length` only
  writes to runtime state. A `--pin` flag that writes to
  `~/.anie/config.toml` (or the nearest project config) via
  the existing
  [`anie-config/src/mutation.rs`](../../crates/anie-config/src/mutation.rs)
  mutator is a nice-to-have. Defer until a user asks.
- **Per-provider default.** "Always use 32 768 on my local
  Ollama, regardless of what `/api/show` says." Could be a
  `[providers.ollama] default_num_ctx = 32_768` block. Defer
  until someone needs it.
- **Batched slash-command syntax.**
  `/context-length 16384 --model qwen3:32b` to set overrides
  for models other than the currently-selected one. Adds
  surface area without clear demand; defer.
- **GC of orphan overrides.** No active cleanup when a model
  disappears from the catalog. Punted as a
  rounding-error-sized concern.
- **Status-bar source marker.** The status bar's denominator
  uses the effective context window in PR 2. A cosmetic marker
  like `ctx 16 384 (override)` can wait; it requires adding a
  source field to `AgentEvent::StatusUpdate`,
  `interactive_mode::apply_status_event`, `StatusBarState`, and
  `render_status_bar`.

## Reference

### anie sites

- Parent plan: `docs/ollama_capability_discovery/README.md` —
  Deferred section at line 880 points to the native-`/api/chat`
  plan which this plan depends on.
- Native-`/api/chat` plan: `docs/ollama_native_chat_api/README.md`.
  This plan is gated on PR 6 of that plan landing.
- Slash-command registration pattern:
  `crates/anie-cli/src/commands.rs:builtin_commands()`.
- `UiAction` dispatch pattern:
  `crates/anie-cli/src/controller.rs:handle_action` (near the
  `SetModel` / `SetThinking` arms).
- Agent-loop request construction:
  `crates/anie-agent/src/agent_loop.rs` (search for
  `StreamOptions`).
- Compaction request construction:
  `crates/anie-cli/src/compaction.rs`.
- Status events:
  `crates/anie-protocol/src/events.rs::AgentEvent::StatusUpdate`,
  `crates/anie-cli/src/controller.rs::status_event`, and
  `crates/anie-tui/src/app.rs` (search for `status_bar`).
- `RuntimeState` persistence:
  `crates/anie-cli/src/runtime_state.rs`.
