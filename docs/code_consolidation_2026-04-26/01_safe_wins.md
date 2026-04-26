# 01 — Safe wins (implemented on this branch)

Low-risk, high-confidence consolidations. Each is mechanical
and doesn't change observable behavior. Together they remove
~200-300 LOC and tighten a few footguns.

## Items in this PR

### 1. Path helper consolidation (F-CONFIG-1)

`anie-config/src/lib.rs` has five public path accessors that
all return `anie_dir().map(|d| d.join(<const>))`. Keep the
named accessors for ergonomic call-site clarity, but route
them through a single private helper:

```rust
fn anie_subpath(suffix: &'static str) -> Option<PathBuf> {
    anie_dir().map(|dir| dir.join(suffix))
}

pub fn global_config_path() -> Option<PathBuf> { anie_subpath("config.toml") }
pub fn anie_auth_json_path() -> Option<PathBuf> { anie_subpath("auth.json") }
pub fn anie_sessions_dir() -> Option<PathBuf> { anie_subpath("sessions") }
pub fn anie_logs_dir() -> Option<PathBuf> { anie_subpath("logs") }
pub fn anie_state_json_path() -> Option<PathBuf> { anie_subpath("state.json") }
```

LOC delta: -25 ish. No call-site changes.

### 2. Atomic-write parent-dir safety (F-CONFIG-2)

`anie-auth/src/lib.rs:388` and `anie-auth/src/store.rs:442`
call `atomic_write` without ensuring the parent dir exists,
while `anie-config/src/mutation.rs:119-121` and
`anie-config/src/lib.rs:539-541` do. Fix the inconsistency
by adding a parent-creating wrapper at the lowest layer:

```rust
// In anie-config:
pub fn atomic_write_create_parent(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    atomic_write(path, contents)
}
```

Update the auth call sites to use the safer wrapper. Keep
the bare `atomic_write` for callers that have already
ensured the parent exists.

### 3. Deprecated `auth_file_path` removal (F-CONFIG-3)

`anie-auth/src/lib.rs:431-432`:
```rust
#[deprecated]
pub fn auth_file_path() -> Option<PathBuf> { default_auth_file_path() }
```

Grep shows no remaining callers. Remove.

### 4. Single-line wrapper inlines on `ControllerState` (F-CLI-3)

In `crates/anie-cli/src/controller.rs`, four wrappers are
called from one place each:
- `session_diff` (line 1087-1089) → inline at line 490
- `session_context` (line 1091-1093) → inline at line 1107
- `context_without_entry` (line 1095-1097) → inline at line 1110
- `list_sessions` (line 1138-1140) → inline at line 514

Keep `current_model_uses_ollama_chat_api` (2 callers, has
shape value). Keep `estimated_context_tokens` (3 callers).

Net: -8 LOC and one less indirection per call site.

### 5. TUI single-line wrapper inlines (F-TUI-4, F-TUI-5)

`crates/anie-tui/src/app.rs:2106` `display_path(&Path) -> String`
called twice — inline `path.display().to_string()` at both
sites.

`crates/anie-tui/src/output.rs:1398-1410` `thinking_gutter_style()`
+ `thinking_body_style()` each called once. Inline.

### 6. `ToolCallResult` re-export chain (F-TUI-6)

`crates/anie-tui/src/lib.rs:17` re-exports `ToolCallResult`
via `app::*`. The type is defined in `output.rs:49`. Drop
the indirection by re-exporting from `output` directly.

### 7. Reasoning-family list dedupe (F-PROV-4)

`local.rs:78` and `model_discovery.rs` both maintain reasoning-
family lists. Move the canonical list into a single function
in `model_discovery.rs` and have `local.rs` call it.

### 8. Cancel + status-event helper (F-CLI-1)

Add `fn cancel_and_notify_status(&mut self)` that runs the
two-step pattern. Apply at the most repetitive arms (5+
sites). Don't try to apply to every arm — some have
intermediate logic that the helper can't capture. Goal is
~30 LOC removed, not exhaustiveness.

### 9. Persistence-warning helper (F-CLI-2)

Add `fn send_persistence_warning_if_present(&self, w:
Option<String>)` and apply at 6 sites in `controller.rs`. ~10
LOC removed.

## What this PR does NOT include

- F-CLI-4 test fixture builder — substantial test churn,
  best done as its own PR with a focused review.
- F-CLI-5 print/RPC/interactive bootstrap — needs careful
  attention to `exit_after_run` semantics; not safe in a
  drive-by.
- F-CLI-6 parametric apply — too speculative without
  measuring how often the shape repeats post-other-changes.
- F-PROV-3 provider-init macro — 30 LOC win not worth a
  workspace-wide macro.
- F-MD-* markdown changes — separate plan.
- F-TUI-1, F-TUI-2, F-TUI-3 — separate plan.

## Test plan

- `cargo test --workspace` — all green; the changes are
  mechanical and behavior-preserving.
- `cargo clippy --workspace --all-targets -- -D warnings` —
  clean.
- No new tests required: each item is either a refactor
  (existing tests cover it) or a removal (no caller, no
  test).

## Exit criteria

- All 9 items implemented or each unimplemented item has a
  documented reason.
- Workspace tests + clippy clean.
- LOC reduction ≥ 200.
- No bench regression.
