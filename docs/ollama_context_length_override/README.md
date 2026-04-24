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
  **effective** `num_ctx`, plus its source (discovered via
  `/api/show`, user override, or the 32 k fallback).
- `/context-length <N>` — set an override for the current
  model. `N` is a positive integer in a reasonable range
  (2 048 ≤ N ≤ 1 048 576). Applied on the next request.
- `/context-length reset` — clear the override for the current
  model. Falls back to discovered → fallback order.
- Non-Ollama (hosted) models reject the command with a
  friendly message: "`/context-length` only applies to Ollama
  models — selected model '{id}' is served by provider
  '{name}'."
- Persisted per-model across sessions.
- TUI status-bar visibility (optional but nice):
  `context_window: 16 384 (override)` in the `/model` view.

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
runtime preference, not a config declaration. Config values
still win if the user wants to hard-pin a value —
`config.toml` remains authoritative.

### Effective-value resolution

Precedence, highest first:

1. **Config override**: if
   `[[providers.<name>.models]]` for this model has
   `context_window = N`, use `N`. (Existing behavior — user-
   authored config already wins at load time.)
2. **Runtime override**: if
   `RuntimeState::ollama_num_ctx_overrides["{provider}:{id}"]`
   exists, use that value.
3. **Discovered**: use `Model.context_window` (populated from
   `/api/show` by Plan 1 PR 6).
4. **Fallback**: 32 768.

The override is resolved in the controller just before it
calls `Provider::stream`: the controller owns both
`ConfigState` and `RuntimeState`, so it's the one place that
can see config-pinned values, runtime overrides, and
discovered values together. The resolved value flows into
`OllamaChatProvider` via a new
`StreamOptions::num_ctx_override: Option<u64>` field. The
provider stays stateless — no map lookups, no controller
pointers — it just reads one `Option<u64>` and writes it to
the body if set, otherwise falls back to
`Model.context_window`. This keeps the provider contract clean
and makes the precedence chain unit-testable in one place
(the controller).

### Command wiring

Slash-command metadata goes through
[`anie-cli/src/commands.rs`](../../crates/anie-cli/src/commands.rs)
— same surface the existing commands use. Dispatch lands in
[`controller.rs`](../../crates/anie-cli/src/controller.rs)
alongside `UiAction::SetModel` / `UiAction::SetThinking`. New
`UiAction` variant:

```rust
UiAction::SetContextLengthOverride(Option<u64>),
// None  → reset
// Some  → set to value (already validated by the argument parser)
```

### Argument parsing

Slash-command argument spec (`ArgumentSpec::FreeForm` with a
validator) living in
[`anie-tui::ArgumentSpec`](../../crates/anie-tui/src/commands.rs):

- Accept `<integer>` or the literal string `reset`.
- Reject integers < 2 048 or > 1 048 576 with a message.
- Reject non-integer / non-`reset` strings with a message
  pointing at the correct usage.

Echoed in the autocomplete-popup hint column as
`/context-length [<N>|reset]`.

### Status-bar / `/model` visibility

Optional but recommended. The status bar already carries
`provider_name` / `model_name`
([`anie-tui/src/app.rs`](../../crates/anie-tui/src/app.rs)
`StatusBarState`); extend with a computed `context_window`
string for Ollama models:

- Discovered unchanged → `ctx 32 768`
- Discovered via `/api/show` → `ctx 262 144 (discovered)`
- Runtime override → `ctx 16 384 (override)`
- Config-pinned → `ctx 8 192 (config)`

Non-Ollama models show nothing extra.

## Files to touch

| File | PR | What |
|------|----|------|
| `crates/anie-cli/src/runtime_state.rs` | 1 | Add `ollama_num_ctx_overrides` field with forward-compat serde |
| `crates/anie-provider/src/options.rs` | 2 | Add `StreamOptions::num_ctx_override: Option<u64>` |
| `crates/anie-cli/src/runtime/config_state.rs` | 2 | Add `effective_num_ctx_override(&RuntimeState) -> Option<u64>` resolver |
| `crates/anie-cli/src/controller.rs` | 2 | Populate `stream_options.num_ctx_override` before calling `Provider::stream` |
| `crates/anie-providers-builtin/src/ollama_chat/convert.rs` | 2 | Prefer `options.num_ctx_override` over `model.context_window` |
| `crates/anie-cli/src/commands.rs` | 3 | Register `/context-length` metadata |
| `crates/anie-tui/src/commands.rs` | 3 | `ArgumentSpec` for `<N>|reset` |
| `crates/anie-cli/src/controller.rs` | 3 | Handle `UiAction::SetContextLengthOverride`; validate + persist |
| `crates/anie-tui/src/app.rs` (StatusBarState) | 4 | Show effective context window + source |
| `crates/anie-cli/src/controller.rs` | 4 | Populate the status-bar field on model switch + override change |

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

### PR 2 — Resolver in the controller + `StreamOptions::num_ctx_override`

**Why second:** no user-facing command yet, but all the plumbing
is in place so PR 3 just needs to flip the stored value. The
resolver lives in the controller (where the `ConfigState` +
`RuntimeState` co-exist), and the provider receives a single
`Option<u64>` via `StreamOptions`. This keeps the precedence
logic in one unit-testable spot and keeps `OllamaChatProvider`
stateless.

**Scope:**

- Extend `StreamOptions` in
  [`anie-provider/src/options.rs:29-40`](../../crates/anie-provider/src/options.rs):

  ```rust
  pub struct StreamOptions {
      // …existing fields…

      /// Override for Ollama's `options.num_ctx`. `None` means
      /// the provider falls back to `Model.context_window`.
      /// Populated by the controller from the effective-value
      /// resolver (see `docs/ollama_context_length_override`).
      /// Ignored by non-Ollama providers.
      pub num_ctx_override: Option<u64>,
  }
  ```

  Additive default-able field; no forward-compat concern.

- New helper in
  [`anie-cli/src/runtime/config_state.rs`](../../crates/anie-cli/src/runtime/config_state.rs)
  (co-located with the state it reads):

  ```rust
  impl ConfigState {
      /// Compute the effective `num_ctx` for the current model.
      /// Returns `None` when the current model isn't Ollama or
      /// when the fallback chain resolves to the catalog's
      /// `Model.context_window` (the provider will use that
      /// directly).
      pub(crate) fn effective_num_ctx_override(
          &self,
          runtime_state: &RuntimeState,
      ) -> Option<u64> { … }
  }
  ```

  Precedence:
  1. User-edited config: if config pins `context_window` for
     this model, it's already baked into
     `Model.context_window` via the catalog-load path →
     return `None` so the provider uses that value directly.
     The catalog *is* the config surface here.
  2. Runtime override (`RuntimeState::ollama_num_ctx_overrides["{provider}:{id}"]`)
     → return `Some(value)`.
  3. Otherwise → return `None` (provider falls back to
     `Model.context_window` which already reflects discovery).

- Controller populates the field just before
  `Provider::stream` at
  [`controller.rs:build_agent`](../../crates/anie-cli/src/controller.rs)
  (or wherever `StreamOptions` is assembled today; one single
  site):

  ```rust
  stream_options.num_ctx_override =
      self.config_state.effective_num_ctx_override(&self.runtime_state);
  ```

- `ollama_chat/convert.rs` body builder prefers
  `options.num_ctx_override` over `model.context_window` when
  present:

  ```rust
  let num_ctx = options.num_ctx_override.unwrap_or(model.context_window);
  body["options"] = json!({ "num_ctx": num_ctx });
  ```

- Non-Ollama providers ignore the field entirely.

**Tests:**

- `effective_num_ctx_override_returns_none_when_no_runtime_entry`
- `effective_num_ctx_override_returns_some_when_runtime_entry_present`
- `effective_num_ctx_override_keyed_by_provider_and_model_tuple`
  (qwen3:32b on `ollama1` and qwen3:32b on `ollama2` track
  separately).
- `effective_num_ctx_override_ignores_non_ollama_current_model`
- `stream_options_num_ctx_override_default_is_none`
- `ollama_chat_body_prefers_num_ctx_override_over_context_window`
- `ollama_chat_body_uses_context_window_when_override_is_none`
- `openai_provider_ignores_num_ctx_override_field`

### PR 3 — `/context-length` command

**Why third:** the user-facing surface. Builds on PR 1 (store)
and PR 2 (consume).

**Scope:**

- Register `/context-length` in
  [`anie-cli/src/commands.rs` `builtin_commands()`
  function](../../crates/anie-cli/src/commands.rs) — pattern
  identical to the existing `/thinking` registration.
- Argument spec in
  [`anie-tui/src/commands.rs::ArgumentSpec`](../../crates/anie-tui/src/commands.rs):
  a custom variant or a `FreeForm` with validation that
  accepts `<positive-integer>` or the literal `reset`.
- Controller handler: new
  `UiAction::SetContextLengthOverride(Option<u64>)` and a
  `handle_action` arm in
  [`controller.rs:handle_action`](../../crates/anie-cli/src/controller.rs)
  (near the existing `UiAction::SetModel` /
  `UiAction::SetThinking` arms).
- Validation — reject non-Ollama models with a friendly
  system message. Reject out-of-range values with a system
  message pointing at the accepted range. Accept `reset` and
  remove the key from the map.
- Persist immediately via the existing
  `ConfigState::persist_runtime_state` path.
- `/context-length` with no args emits a system message:
  `"Current context window: 262 144 (discovered via /api/show)"`
  or `"Current context window: 16 384 (runtime override)"` —
  format mirrors `/thinking`'s feedback.

**Tests:**

- `context_length_command_registered_with_expected_arg_spec`
- `context_length_sets_override_for_current_ollama_model`
- `context_length_reset_clears_override`
- `context_length_on_non_ollama_model_emits_friendly_error`
- `context_length_rejects_out_of_range_value`
- `context_length_rejects_unparseable_argument`
- `context_length_no_args_reports_current_effective_value_and_source`
- `context_length_override_persists_across_session_restart`
- `context_length_override_applies_to_next_request_without_reload`
  (integration test via mock Ollama server: send two requests
  with an override in between, assert the second request body
  has the new `num_ctx`).

### PR 4 — Status-bar visibility

**Why last:** pure UI polish; doesn't block the feature.

**Scope:**

- Extend `StatusBarState` in
  [`anie-tui/src/app.rs`](../../crates/anie-tui/src/app.rs)
  with a `context_window_hint: Option<String>` field.
- Populate on model switch + on override change in the
  controller.
- Render next to provider/model in the status bar. For
  non-Ollama models, `context_window_hint` is `None` and
  nothing is shown.

**Tests:**

- `status_bar_shows_discovered_context_window_for_ollama`
- `status_bar_shows_override_source_when_runtime_override_active`
- `status_bar_shows_config_source_when_config_pinned`
- `status_bar_omits_context_window_hint_for_non_ollama_models`

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
| Manual | Status bar shows `ctx <value> (source)`; switch models; value updates. | smoke |
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
- **Config file's `context_window` beats runtime override.**
  Intentional, but could confuse a user who set a config
  value years ago and forgot. PR 3's no-arg
  `/context-length` output should note the effective source
  ("config", "override", "discovered", "fallback") so the
  user can see at a glance why their override didn't stick.
- **Tool autocomplete list grows.** Minor — one more entry.
  Well within the existing UI budget.

## Exit criteria

- [ ] PR 1 merged: `RuntimeState.ollama_num_ctx_overrides`
      field exists with forward-compat serde.
- [ ] PR 2 merged: resolver correctly composes
      override + discovered + fallback in that precedence.
- [ ] PR 3 merged: `/context-length` command works end-to-end
      with all four shapes (set, reset, query, error).
- [ ] PR 4 merged: status bar shows the effective value and
      source.
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
- `StatusBarState`:
  `crates/anie-tui/src/app.rs` (search for `status_bar`).
- `RuntimeState` persistence:
  `crates/anie-cli/src/runtime_state.rs`.
