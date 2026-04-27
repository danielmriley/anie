# 02 — Load `[ui]` settings through the real config path

## Rationale

`AnieConfig` exposes `ui: UiConfig`, and interactive startup reads it:

- `crates/anie-config/src/lib.rs:41-43` — `AnieConfig::ui`.
- `crates/anie-config/src/lib.rs:140-163` — `UiConfig` fields.
- `crates/anie-cli/src/interactive_mode.rs:35-53` — startup wires
  `slash_command_popup_enabled`, `markdown_enabled`, and
  `tool_output_mode` into `App`.

But `load_config_with_paths()` parses TOML through `PartialAnieConfig`,
and the partial type currently has no `ui` field:

- `crates/anie-config/src/lib.rs:795-809` — `PartialAnieConfig` has
  `model`, `providers`, `compaction`, `context`, `tools`, and `ollama`,
  but not `ui`.
- `crates/anie-config/src/lib.rs:699-780` — `merge_partial_config()`
  cannot merge `[ui]` because the partial data is never captured.

Existing tests deserialize `UiConfig` directly, which proves serde for
that struct, but not the real config loading path.

## Design

Add partial config support for `[ui]` using the same merge pattern as
`[tools.bash.policy]` and `[ollama]`:

```rust
#[derive(Debug, Default, Deserialize)]
struct PartialAnieConfig {
    // existing fields...
    #[serde(default)]
    ui: Option<PartialUiConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialUiConfig {
    slash_command_popup_enabled: Option<bool>,
    markdown_enabled: Option<bool>,
    tool_output_mode: Option<ToolOutputMode>,
}
```

Then merge each `Some` value into `config.ui` without disturbing
omitted fields.

Keep the public `UiConfig` shape unchanged. These are UI-only
preferences and should remain outside session/provider state.

## Files to touch

- `crates/anie-config/src/lib.rs`
  - Add `PartialUiConfig`.
  - Add `ui` to `PartialAnieConfig`.
  - Merge optional UI fields in `merge_partial_config()`.
  - Add real-loader tests.
  - Optionally update `default_config_template()` with a commented
    `[ui]` example.
- Optional docs updates:
  - `docs/notes/commands_and_slash_menu.md`
  - `docs/ROADMAP.md`
  - Any user-facing config docs that already mention UI settings.

## Phased PRs

### PR A — Merge `[ui]` from config files

**Change:**

- Add `PartialUiConfig` and merge support.
- Add tests that call `load_config_with_paths()` with TOML containing:

```toml
[ui]
slash_command_popup_enabled = false
markdown_enabled = false
tool_output_mode = "compact"
```

**Tests:**

- `ui_config_loads_from_real_config_path`
- `partial_ui_config_preserves_defaults_for_omitted_fields`
- Existing direct `UiConfig` serde tests.

**Exit criteria:**

- Real config loading changes `config.ui` as expected.
- Omitting `[ui]` still produces `UiConfig::default()`.

### PR B — Template/documentation discoverability

**Change:**

- Add a commented `[ui]` section to `default_config_template()`.
- Keep defaults documented inline:

```toml
# [ui]
# slash_command_popup_enabled = true
# markdown_enabled = true
# tool_output_mode = "verbose" # "verbose" or "compact"
```

**Tests:**

- Extend the existing default-template test or add
  `default_template_documents_ui_block`.

**Exit criteria:**

- A new user opening `~/.anie/config.toml` can discover the UI knobs.

## Test plan

- `cargo test -p anie-config`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke:
  - Set `[ui] markdown_enabled = false`, start TUI, confirm finalized
    assistant blocks render plain.
  - Set `[ui] tool_output_mode = "compact"`, run a successful `read` or
    `bash` tool call, confirm body is hidden in the transcript.

## Risks

- `ToolOutputMode` uses lowercase serde names. Keep tests around
  `"compact"` and `"verbose"` so future enum changes do not break user
  config silently.
- Avoid treating an empty `[ui]` table as an error; it should behave like
  omitted config and preserve defaults.

## Exit criteria

- `[ui]` settings work through `load_config_with_paths()`.
- Interactive startup consumes loaded values without additional changes.
- Defaults remain backward compatible.

## Deferred

- Persisting UI preferences at runtime. This plan only makes config-file
  values work.
