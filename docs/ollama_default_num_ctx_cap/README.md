# Default `num_ctx` cap for Ollama

**Add an opt-in workspace-level cap on the `num_ctx` value sent
to Ollama, applied as a hard ceiling on top of
`Model.context_window`. Belt-and-suspenders defense for users
on constrained hardware who don't want to manage per-model
overrides via `/context-length`.**

## Context

This plan is preventive; its sibling
[`../ollama_load_failure_recovery/README.md`](../ollama_load_failure_recovery/README.md)
is reactive. Together they cover the gap created when
[`docs/ollama_native_chat_api/README.md`](../ollama_native_chat_api/README.md)
PR 6 and the `local-probe` follow-up made anie send the full
architectural `num_ctx` from `/api/show` on every Ollama
request:

- Reactive (recovery plan): on a memory-related load failure,
  retry once with halved `num_ctx` and surface an actionable
  `/context-length` hint.
- Preventive (this plan): on a known-constrained system, never
  send a request larger than a user-declared cap in the first
  place — saves the multi-second reload-then-fail-then-reload
  cycle.

The recovery plan covers the unhappy-path UX. This cap is for
users who say "I have a 16 GB Mac and I'd rather anie default
to a sane window than discover the cap empirically." Opt-in,
documented; no change for existing setups.

### When this matters

Mostly first-time / fresh-hardware setups with large models:

- Mac mini / 16 GB Mac with a 32B+ model and `num_ctx =
  262 144` from `/api/show` → KV cache demand exceeds
  available RAM at load time.
- A homelab box with 24 GB VRAM trying to run several
  parallel anie sessions against the same Ollama — each
  session's KV demand is independent from anie's perspective
  but adds up on the server.

For users with abundant memory the default behavior is
correct and this plan adds nothing. The cap is unset by
default; users who want it set it once in
`~/.anie/config.toml`.

## Design

### Config field

Add a single optional field. Top of
[`AnieConfig`](../../crates/anie-config/src/lib.rs):

```toml
[ollama]
default_max_num_ctx = 32768
```

Internally:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OllamaConfig {
    /// Hard ceiling on `num_ctx` sent to Ollama, regardless of
    /// what `/api/show` reports. `None` (the default) means no
    /// cap; the model's discovered architectural max applies.
    /// When set, `Model.context_window` for any
    /// `OllamaChatApi` model is clamped at catalog-load time
    /// to `min(discovered, default_max_num_ctx)`.
    ///
    /// Distinct from `/context-length`'s per-model runtime
    /// override: this is a workspace-level safety floor, the
    /// override is a per-model fine-grain control. The runtime
    /// override always wins over the cap; if the user
    /// explicitly types `/context-length 65536` on a session
    /// with `default_max_num_ctx = 32768`, the override
    /// applies (with a one-line system message noting the
    /// explicit override).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_max_num_ctx: Option<u64>,
}
```

The field lives under a new `[ollama]` block (not under
`[compaction]` or `[providers.ollama]`) because:

- It applies to every Ollama provider, not a single one.
- It's not a compaction concern — compaction reads
  `effective_ollama_context_window`, which already accounts
  for runtime overrides; this cap just lowers the input.
- A future `[ollama] keep_alive`, `[ollama]
  default_temperature`, etc. will fit the same block.

### Where to apply the cap

Apply at **catalog load time**, in
[`anie-provider/src/model.rs::to_model`](../../crates/anie-provider/src/model.rs)
and the local-probe path
([`anie-providers-builtin/src/local.rs::probe_openai_compatible`](../../crates/anie-providers-builtin/src/local.rs)).
Both sites already participate in deciding
`Model.context_window`; this plan adds one extra clamp:

```rust
// In to_model, after computing the discovered context_window:
let context_window = if api == ApiKind::OllamaChatApi {
    self.context_length
        .map(|discovered| match default_max_num_ctx {
            Some(cap) => discovered.min(cap),
            None => discovered,
        })
        .unwrap_or(32_768)
} else { /* existing branches */ };
```

Applying at catalog-load time means:
- The TUI's status bar and `/model` view show the capped
  value (matches what's sent on the wire).
- Compaction triggers based on the capped value.
- The runtime override resolver's "no override → use
  `model.context_window`" path naturally picks up the cap.

### Precedence

Already covered briefly in `OllamaConfig::default_max_num_ctx`
docstring. Restated:

1. **Runtime override** (`/context-length <N>`) — wins
   absolutely. The user explicitly typed this.
2. **Cap** (`[ollama] default_max_num_ctx`) — applied to
   `Model.context_window` at catalog load.
3. **Discovered** (`/api/show.context_length`) — the source
   the cap is applied to.
4. **Fallback** — 32 768 when neither cap nor discovery
   produces a value.

Precedence is enforced by the order in which values are
written: cap clamps the catalog value at load time; runtime
override is consumed at request build time and shadows the
catalog value entirely.

### Threading the cap

The cap value lives on `AnieConfig`. To reach the catalog-load
path:

- `to_model` doesn't know about `AnieConfig` today (it takes
  `(api, base_url)` only). Threading the full config through
  is overkill. Cleanest: pass the cap as an explicit
  `Option<u64>` parameter, computed by the caller.

```rust
impl ModelInfo {
    pub fn to_model(
        &self,
        api: ApiKind,
        base_url: &str,
        ollama_default_max_num_ctx: Option<u64>,
    ) -> Model { … }
}
```

Callers (`model_discovery::discover_ollama_tags`,
`local::probe_openai_compatible`, the catalog-build path in
[`anie-cli/src/model_catalog.rs`](../../crates/anie-cli/src/model_catalog.rs))
all have the `AnieConfig` already in scope. They pass
`config.ollama.default_max_num_ctx` through.

For `local::probe_openai_compatible`, the function doesn't
currently take `AnieConfig` either. Add the cap as an explicit
parameter on the helper that wraps the per-model upgrade
loop, and at the public-API boundary the caller (in
`bootstrap.rs` / `detect_local_servers`) passes it through.

### `/context-length` interaction

User behavior:

- `/context-length` (no args) reports the **effective** value.
  If the cap is in effect, the message reads:

  ```
  Current context window: 32768 (capped from 262144 via
  [ollama] default_max_num_ctx)
  ```

  When no cap is set the message is unchanged.

- `/context-length <N>` accepts any value in the existing
  range (2048–1048576). If `N > default_max_num_ctx`, the
  override still applies, but the system message notes:

  ```
  Context window override set to 65536. Note: this exceeds
  your [ollama] default_max_num_ctx of 32768. Models that
  cannot fit at 65536 will surface a load failure.
  ```

  Honors user intent (they typed it explicitly) while making
  the conflict visible.

- `/context-length reset` falls back to
  `Model.context_window` — which is already capped.

## Files to touch

| File | PR | What |
|------|----|------|
| `crates/anie-config/src/lib.rs` | 1 | Add `OllamaConfig { default_max_num_ctx: Option<u64> }`; serde round-trip; default-template comment |
| `crates/anie-provider/src/model.rs` | 2 | Add `ollama_default_max_num_ctx: Option<u64>` parameter to `to_model`; clamp `context_window` for `OllamaChatApi` |
| `crates/anie-providers-builtin/src/model_discovery.rs` | 2 | Pass the cap through to `to_model` from `discover_ollama_tags` |
| `crates/anie-providers-builtin/src/local.rs` | 2 | Same threading for `probe_openai_compatible`; pass cap to the local-probe upgrade loop and clamp before assigning `model.context_window` from `show.context_length` |
| `crates/anie-cli/src/model_catalog.rs` | 2 | Pass `config.ollama.default_max_num_ctx` from `AnieConfig` into the discovery callsites |
| `crates/anie-cli/src/controller.rs` (handle_action) | 3 | Update `/context-length` no-args message to mention the cap when in effect; emit explicit-override warning when `N > cap` |
| `crates/anie-cli/src/runtime/config_state.rs` | 3 | Helper for the no-args message: `effective_ollama_context_window_with_source()` returns `(value, source)` where source ∈ `{Override, Capped(discovered, cap), Discovered, Fallback}` |

## Phased PRs

### PR 1 — Add `[ollama]` config block + `default_max_num_ctx` field

**Why first:** schema only, no behavior change. Lets users
declare the cap in `config.toml` without anie reading it
yet — guarantees forward-compat: a config file with the cap
loads cleanly even on builds before PR 2 lands.

**Scope:**

- `anie-config/src/lib.rs`:
  - Add `OllamaConfig` struct with `default_max_num_ctx:
    Option<u64>`.
  - Add `pub ollama: OllamaConfig` to `AnieConfig` with
    `#[serde(default)]`.
  - Update `default_config_template()` to include a commented
    `[ollama]` block as a discoverable example.
- Tests:
  - `ollama_config_default_max_num_ctx_round_trips_serde`
  - `anie_config_loads_when_ollama_block_is_absent`
  - `anie_config_loads_when_default_max_num_ctx_is_absent`
  - `default_template_documents_ollama_block_with_example`

**Exit criteria:**

- A user can write `[ollama] default_max_num_ctx = 32768` in
  their config and the file loads without error.
- The value is round-trippable through TOML → struct → TOML.
- No code reads the value yet.

### PR 2 — Apply the cap at catalog load time

**Why second:** the actual behavior change. After this PR,
`Model.context_window` reflects the cap, the TUI status bar
shows the capped value, compaction triggers based on it, and
the wire request to Ollama uses the capped `num_ctx`.

**Scope:**

- `anie-provider/src/model.rs`:
  - Extend `to_model` signature with
    `ollama_default_max_num_ctx: Option<u64>`.
  - Clamp `context_window` for `OllamaChatApi` per the design
    above. Other ApiKinds ignore the cap.
- `anie-providers-builtin/src/model_discovery.rs`:
  - `discover_ollama_tags` and the surrounding callsites:
    pass the cap from caller-supplied context. The
    `ModelDiscoveryRequest` type may need a new field or the
    cap can flow as an explicit parameter on
    `discover_models`. PR design picks the cleaner of the
    two; both are <10 lines.
- `anie-providers-builtin/src/local.rs`:
  - `probe_openai_compatible` and the post-loop upgrade get
    a cap parameter. Apply the clamp at the same site as the
    `model.context_window = ctx` assignment from the
    `local-probe` follow-up commit.
- `anie-cli/src/model_catalog.rs`:
  - Read `config.ollama.default_max_num_ctx` and pass it
    into the discovery / probe paths.
- Tests:
  - `to_model_clamps_ollama_context_window_when_cap_is_set`
  - `to_model_preserves_discovered_when_cap_exceeds_discovered`
    (cap = 1 048 576, discovered = 262 144 → result =
    262 144).
  - `to_model_ignores_cap_for_non_ollama_chat_api`
  - `local_probe_clamps_show_context_length_when_cap_is_set`
  - `model_discovery_propagates_cap_to_to_model`
  - **End-to-end:**
    `anie_models_command_shows_capped_value_when_config_sets_cap`

**Exit criteria:**

- With `[ollama] default_max_num_ctx = 32768` set, all
  Ollama models with discovered context > 32 768 show
  `context_window = 32 768` in `anie models`.
- The wire request reflects the cap (verifiable via
  `/api/ps` after the request).
- Without the config field, behavior is unchanged from the
  current state.

### PR 3 — Surface the cap in `/context-length` messaging

**Why third:** UX polish; users who set the cap should
understand when it's in effect and see a friendly warning if
they explicitly override above it.

**Scope:**

- `anie-cli/src/runtime/config_state.rs`:
  - Add `effective_ollama_context_window_with_source(&self) ->
    (u64, ContextWindowSource)` where
    `ContextWindowSource` is `enum { RuntimeOverride,
    Capped { discovered: u64, cap: u64 }, Discovered,
    Fallback }`.
  - Existing `effective_ollama_context_window()` continues to
    return just the value for callers that don't need the
    source.
- `anie-cli/src/controller.rs`:
  - `/context-length` no-args reads the source and formats
    accordingly:

    ```
    No cap:                  "Current context window: 262144"
    With cap, capped:        "Current context window: 32768
                              (capped from 262144 via
                              [ollama] default_max_num_ctx)"
    With override:           "Current context window: 16384
                              (runtime override; baseline
                              32768)"
    With override > cap:     "Current context window: 65536
                              (runtime override; baseline
                              32768; exceeds [ollama] cap of
                              32768)"
    ```

  - `/context-length <N>` set-action: when `N > cap` (and a
    cap is set), emit a one-line warning system message but
    still apply the override. The warning text mirrors the
    last bullet above.
- Tests:
  - `effective_ollama_context_window_with_source_no_cap_no_override`
  - `effective_ollama_context_window_with_source_capped_no_override`
  - `effective_ollama_context_window_with_source_override_within_cap`
  - `effective_ollama_context_window_with_source_override_exceeds_cap`
  - `context_length_no_args_message_includes_cap_when_capped`
  - `context_length_set_above_cap_emits_warning_but_applies_override`

**Exit criteria:**

- Users with the cap set understand when it's active.
- Users overriding above the cap see a warning, and the
  override still applies.

## Test plan

Per-PR tests above. Cross-cutting:

| # | Test | Where |
|---|------|-------|
| Manual | Set `[ollama] default_max_num_ctx = 32768` in `~/.anie/config.toml`. Run `anie models --provider ollama --refresh`. Expect every Ollama model with discovered context > 32 768 to display `32 768`. | smoke |
| Manual | With cap set, send a request to `qwen3.5:0.8b` (discovered 262 144). Verify `/api/ps` reports `context_length = 32 768`. | smoke |
| Manual | With cap set, run `/context-length 65536` (above cap). Verify the warning message and that the next request's `num_ctx` is 65 536 (override wins). | smoke |
| Manual | Without cap (default), behavior should be identical to today. | regression |
| Auto | `cargo test --workspace --no-fail-fast` green. | CI |
| Auto | `cargo clippy --workspace --all-targets -- -D warnings` clean. | CI |

## Risks

- **Cap propagation lag during in-memory catalog rebuild.**
  If the user changes `default_max_num_ctx` in `config.toml`
  during a session (`/reload` reloads config), the catalog
  rebuilds and applies the new cap. In-flight runs use the
  pre-reload value; documented.
- **Cap below `/context-length` minimum (2048).** The cap
  itself should be ≥ 2048 to avoid producing unusable models.
  PR 1 should validate this on parse and reject configs with
  `default_max_num_ctx < 2048` with a clear error.
- **TUI display drift.** The status bar shows
  `Model.context_window`. With the cap applied at to_model,
  the displayed value matches the wire value — no drift.
- **Override-above-cap surprise.** Mitigated by the explicit
  warning in PR 3. The user's intent is preserved (they typed
  it); the warning makes the conflict visible.
- **First-run experience.** A user with the cap set hits
  no surprise: cap applies, models display the capped value,
  requests succeed. A user without the cap continues to hit
  the existing failure modes — which the recovery plan
  handles.

## Exit criteria

- [ ] PR 1 merged: `[ollama] default_max_num_ctx` is a valid
      config field and round-trips through serde.
- [ ] PR 2 merged: when set, the cap clamps every Ollama
      model's `context_window` at catalog load. `anie models`
      and the wire request both reflect the cap.
- [ ] PR 3 merged: `/context-length` messaging discloses the
      cap when in effect and warns on explicit overrides
      above the cap.
- [ ] Cross-cutting smoke (manual + regression) passes.
- [ ] `cargo test --workspace --no-fail-fast` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] No anie-specific deviation from pi unflagged: pi
      doesn't have a native Ollama codepath at all, so the
      whole `[ollama]` config block is anie-specific.
      Commented at the type definition per CLAUDE.md §3.

## Deferred

- **Hardware probing.** anie could detect `nvidia-smi` /
  `sysctl hw.memsize` / Apple Silicon unified memory and
  auto-suggest a cap on first run. Adds platform-specific
  code paths and only mostly-correct heuristics. Defer until
  a user asks.
- **Per-model cap.** A future
  `[providers.ollama.models]` `max_num_ctx_override = N`
  would let users cap individual models below the workspace
  default. The runtime override already covers this
  point-in-time; persistent per-model caps require a config
  schema bump. Defer.
- **Cap differences for native vs. OpenAI-compat Ollama.**
  Legacy `OpenAICompletions`-tagged Ollama entries use the
  32 k regression-guard fallback; the cap is a no-op for
  them. If we ever migrate the legacy path, the cap would
  apply uniformly. Documented; no work.
- **Auto-clamp based on observed Ollama load failures.** If
  the user repeatedly hits load failures, anie could persist
  a learned cap in `RuntimeState`. Adaptive behavior is
  surprising; a manual `[ollama] default_max_num_ctx` is
  more predictable. Defer.

## Reference

### anie sites

- Sibling plan (reactive recovery):
  `docs/ollama_load_failure_recovery/README.md`.
- Native `/api/chat` codepath:
  `docs/ollama_native_chat_api/README.md` PR 6 — flipped the
  context-window regression guard.
- `/context-length` slash command:
  `docs/ollama_context_length_override/README.md` — the
  per-model runtime override that wins over the cap.
- Catalog-load entry point:
  `crates/anie-cli/src/model_catalog.rs::build_model_catalog`.
- `to_model` site for the clamp:
  `crates/anie-provider/src/model.rs::to_model`.
- Local-probe site for the clamp:
  `crates/anie-providers-builtin/src/local.rs::probe_openai_compatible`
  (the `Ok(Some(show))` arm where
  `model.context_window = ctx` was added in commit
  `8714b33`).
