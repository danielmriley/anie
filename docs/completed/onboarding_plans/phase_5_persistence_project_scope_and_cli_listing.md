# Phase 5 — Persistence, Project Scope, and CLI Model Listing

This phase finalizes where model selections are persisted and adds a non-interactive CLI surface for listing available models.

## Why this phase exists

Without this phase, model picks from `/model` and onboarding work at runtime but the user has no confidence about:

- **where** the pick was saved (global? project? session-only?)
- **whether** newly available models are visible from the CLI without launching the TUI

This phase also aligns with `tmp.md`'s request for an `anie config --models` style command.

---

## Current code facts

### Config write paths

| Writer | Target | Notes |
|--------|--------|-------|
| `write_configured_providers()` | global `~/.anie/config.toml` | used by onboarding |
| `ConfigMutator` | any explicit path | used by onboarding + provider management |
| `ControllerState::set_model()` | session + runtime state only | used by `/model` text command today |
| `ControllerState::reload_config()` | reads merged config | used after onboarding/provider changes |

### Config discovery

```rust
// crates/anie-config/src/lib.rs
pub fn global_config_path() -> Option<PathBuf>;
pub fn find_project_config(start: &Path) -> Option<PathBuf>;
pub fn load_config_with_paths(global, project, overrides) -> Result<AnieConfig>;
```

### Current model-switch behavior

When `UiAction::SetModel(requested)` is handled:

1. `resolve_requested_model()` looks up the model in the catalog
2. `session.append_model_change()` records it in the session JSONL
3. `persist_runtime_state()` writes to `~/.anie/state.json`

No config file is changed. This is session/runtime-only persistence.

### Current CLI subcommand surface

```rust
pub enum Command {
    Onboard,
}
```

Only `anie onboard` exists today.

---

## Files expected to change

### New files

- `crates/anie-cli/src/models_command.rs` — CLI model-listing logic

### Modified files

- `crates/anie-cli/src/lib.rs` — add `Models` subcommand variant
- `crates/anie-cli/src/controller.rs` — optional: config-write on model selection
- `crates/anie-config/src/lib.rs` — add `preferred_write_target()` helper
- `crates/anie-tui/src/app.rs` — show write-target in status messages

---

## Important design decision

### Where should `/model` selection persist?

**Recommended policy for v1:**

| Context | Write target | Rationale |
|---------|-------------|-----------|
| Onboarding (`anie onboard`) | global `~/.anie/config.toml` | machine-level setup |
| Provider management (`/providers`) | global `~/.anie/config.toml` | provider defaults are global |
| `/model` in interactive TUI | session + runtime state (no config file change) | ephemeral per-session choice |
| `/model` with explicit persist flag (future) | nearest project or global config | explicit opt-in |

Why session-only for `/model` by default:

- pi also persists model selections to settings (global), but Anie already has a strong session/runtime-state system
- model picks are already recorded in the session JSONL, so `anie --resume` restores them
- writing to config on every `/model` would cause unexpected config churn for users who frequently switch models

If users request durable `/model` persistence later, it can be added as an explicit command variant like `/model set-default <id>` or a settings toggle.

---

## Recommended implementation

### Sub-step A — Add `preferred_write_target()` helper

In `crates/anie-config/src/lib.rs`:

```rust
/// Determine the preferred config file for writing provider/model defaults.
///
/// Returns the nearest existing project `.anie/config.toml` if found,
/// otherwise the global config path.
pub fn preferred_write_target(cwd: &Path) -> Option<PathBuf> {
    find_project_config(cwd).or_else(global_config_path)
}
```

This helper is used by onboarding and provider management when they persist config changes. It is **not** used by `/model` (which stays session-only in v1).

### Sub-step B — Update status messages for config writes

When onboarding or provider management writes to a config file, the status/system message should include the target:

```
Saved configuration to ~/.anie/config.toml
```

or:

```
Saved configuration to ./.anie/config.toml
```

This helps the user understand where their changes went.

### Sub-step C — Add `anie models` CLI subcommand

Add to `crates/anie-cli/src/lib.rs`:

```rust
pub enum Command {
    Onboard,
    Models {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        refresh: bool,
    },
}
```

Behavior of `anie models`:

1. load config (global + project)
2. build the credential store
3. for each configured provider (or the one specified by `--provider`):
   a. build a `ModelDiscoveryRequest` from config + credentials
   b. call `discover_models()` (or `get_or_discover()` if using cached results)
4. print a table of discovered models:

```
Provider    Model ID               Context    Reasoning  Images
──────────  ─────────────────────  ─────────  ─────────  ──────
anthropic   claude-sonnet-4-6      1,000,000  ✓          ✓
anthropic   claude-opus-4-6        1,000,000  ✓          ✓
anthropic   claude-haiku-4-5       200,000    ✓          ✓
openai      gpt-4o                 128,000               ✓
openai      o4-mini                200,000    ✓          ✓
ollama      qwen3:32b              32,768     ✓
ollama      qwen3:8b               32,768     ✓
```

Optional flags:

- `--provider openai` → filter to one provider
- `--refresh` → bypass cache

If no providers are configured, print a helpful message:

```
No providers configured. Run `anie onboard` to set up a provider.
```

### Sub-step D — Implement `models_command.rs`

```rust
pub async fn run_models_command(
    provider_filter: Option<&str>,
    refresh: bool,
) -> Result<()> {
    // 1. Load config
    // 2. Build credential store
    // 3. For each matching provider, build discovery request
    // 4. Discover models (cached or refreshed)
    // 5. Print formatted table to stdout
}
```

### Sub-step E — Wire the subcommand in `run()`

In `crates/anie-cli/src/lib.rs`:

```rust
if let Some(Command::Models { provider, refresh }) = &cli.command {
    return models_command::run_models_command(
        provider.as_deref(),
        *refresh,
    ).await;
}
```

### Sub-step F — Ensure `/model` stays session-only

Verify that the `/model` handler in `app.rs` does **not** write to any config file. The controller's `set_model()` should continue to write only to:

- the session JSONL (via `session.append_model_change()`)
- the runtime state file (via `persist_runtime_state()`)

This is the existing behavior and should be preserved, not accidentally changed by the picker integration.

---

## Constraints

1. **`/model` does not write to config files** in v1. It stays session/runtime-only.
2. **Onboarding and provider management use `preferred_write_target()`** for config writes.
3. **`anie models` must work without a TUI** — it is a print-mode command.
4. **`anie models` must reuse the Phase 1 discovery service and cache.**
5. **Config writes must use `ConfigMutator`** to preserve comments.
6. **Status messages must state the write target** so the user knows which file changed.

---

## Test plan

### Required unit tests

| # | Test |
|---|------|
| 1 | `preferred_write_target()` returns project config when it exists |
| 2 | `preferred_write_target()` falls back to global when no project config |
| 3 | `anie models` subcommand parses correctly |
| 4 | `anie models --provider openai` filters to one provider |
| 5 | `/model` does not modify any config file (session-only) |

### Required CLI tests

| # | Test |
|---|------|
| 1 | `anie models` prints table header and at least one model when config exists |
| 2 | `anie models` prints helpful message when no providers configured |
| 3 | `anie models --refresh` bypasses cache |

### Manual validation

1. run `anie models` with Ollama running → verify local models listed
2. run `anie models --provider anthropic` with API key → verify Anthropic models listed
3. use `/model` in TUI → verify `config.toml` is **not** modified
4. use `/providers` → set default → verify config file change + correct status message

---

## Exit criteria

- [ ] `preferred_write_target()` exists and is tested
- [ ] status messages for config writes include the target file path
- [ ] `anie models` subcommand exists and prints discovered models
- [ ] `anie models` supports `--provider` filter and `--refresh`
- [ ] `/model` stays session-only (no config file writes)
- [ ] all tests pass

---

## Follow-on phase

→ `phase_6_polish_docs_and_validation.md`
