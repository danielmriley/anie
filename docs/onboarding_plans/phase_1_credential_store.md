# Phase 1 — CredentialStore with `keyring`

This phase adds the `keyring` dependency and builds the `CredentialStore` abstraction inside `anie-auth`.  No existing behavior changes yet — the old JSON path continues to work.

## Why this phase exists

The current credential storage writes API keys to `~/.anie/auth.json` in plaintext.  The `keyring` crate provides access to each platform's encrypted credential store (macOS Keychain, Windows Credential Manager, Linux Secret Service) and is the industry-standard approach for CLI tools.

This phase introduces the new `CredentialStore` type alongside the existing `AuthStore` / `save_api_key` code, without replacing it.  Subsequent phases will migrate callers.

---

## Files expected to change

### Primary

- `crates/anie-auth/Cargo.toml` — add `keyring` dependency with platform features.
- `crates/anie-auth/src/store.rs` — new file containing `CredentialStore`.
- `crates/anie-auth/src/lib.rs` — re-export `CredentialStore`; existing code stays.

### No changes yet

- `crates/anie-cli/src/onboarding.rs` — remains on old `save_api_key()` until Phase 2.
- `crates/anie-tui/` — no TUI work yet.
- `crates/anie-config/` — no config schema changes.

---

## Sub-steps

### Sub-step A — Add `keyring` dependency

In `crates/anie-auth/Cargo.toml`, add:

```toml
[dependencies]
keyring = { version = "3", features = [
    "apple-native",
    "windows-native",
    "sync-secret-service",
] }
```

Consider making `keyring` optional behind a `keyring-native` feature flag so that headless / CI builds can skip it:

```toml
[features]
default = ["keyring-native"]
keyring-native = ["dep:keyring"]
```

Verify the workspace `Cargo.lock` resolves cleanly and the crate compiles on the current dev platform.

### Sub-step B — Define `CredentialStore`

Create `crates/anie-auth/src/store.rs` with the following public API:

```rust
pub struct CredentialStore { /* ... */ }

impl CredentialStore {
    /// Create a store using the default app name ("anie") and the default
    /// JSON fallback path (~/.anie/auth.json).
    pub fn new() -> Self;

    /// Create a store with a custom app name and fallback path (for tests).
    pub fn with_config(app_name: &str, json_fallback: PathBuf) -> Self;

    /// Retrieve a credential for the given provider.
    /// Resolution order: keyring → JSON fallback.
    pub fn get(&self, provider: &str) -> Option<String>;

    /// Store a credential.  Writes to keyring first; optionally mirrors to JSON.
    pub fn set(&self, provider: &str, key: &str) -> Result<()>;

    /// Delete a credential from both keyring and JSON.
    pub fn delete(&self, provider: &str) -> Result<()>;

    /// List all provider names that have a stored credential.
    pub fn list_providers(&self) -> Vec<String>;
}
```

**Implementation notes:**

- Use `keyring::Entry::new("anie", provider)` for the keyring handle.
- On `get()`, try `entry.get_password()` first.  If keyring access fails (unsupported platform, daemon not running), fall through to JSON.
- On `set()`, attempt `entry.set_password()`.  If keyring fails, log a warning and write to JSON as fallback.  If keyring succeeds, also write to JSON for portability (this can be made configurable later).
- On `delete()`, attempt both keyring and JSON removal.  Tolerate either failing.
- `list_providers()` merges keys from keyring (not directly enumerable on all platforms) with keys from the JSON file.  Initially this can rely on the JSON file as the source of truth for names, since `set()` mirrors to JSON.

### Sub-step C — Conditional compilation for `keyring` feature

Guard all `keyring::Entry` calls behind `#[cfg(feature = "keyring-native")]`.  When the feature is disabled, `CredentialStore` degrades to pure JSON storage.  This ensures:

- CI environments without a secret service daemon can still build and test.
- Docker / headless server deployments work without extra dependencies.

### Sub-step D — Re-export from `lib.rs`

In `crates/anie-auth/src/lib.rs`, add:

```rust
mod store;
pub use store::CredentialStore;
```

The existing `AuthStore`, `save_api_key`, `load_auth_store`, and `AuthResolver` remain unchanged and publicly exported.

### Sub-step E — Unit tests for `CredentialStore`

Add tests in `crates/anie-auth/src/store.rs`:

1. **Round-trip via JSON fallback** (runs everywhere):
   - Create a `CredentialStore::with_config(...)` pointing at a `tempdir`.
   - `set("openai", "sk-test")` → `get("openai")` → assert `Some("sk-test")`.
   - `delete("openai")` → `get("openai")` → assert `None`.

2. **`list_providers` returns stored names**:
   - Set multiple providers → assert `list_providers()` contains all names.

3. **Keyring integration** (gated behind `#[cfg(feature = "keyring-native")]` and `#[ignore]` for CI):
   - Same round-trip as above but verify that the keyring path is exercised.
   - Clean up test entries in a drop guard.

4. **Fallback when keyring is unavailable**:
   - If possible, simulate keyring failure (e.g., invalid app name) and verify JSON fallback is used.

---

## Constraints

1. **Do not remove or modify** existing `AuthStore`, `save_api_key`, `load_auth_store`, or `AuthResolver`.  Phase 2 handles migration.
2. **Do not touch** onboarding, CLI, or TUI code.
3. The JSON fallback file must continue to use `0600` permissions on Unix.
4. The `CredentialStore` must be `Send + Sync` for use in async contexts.

---

## Test plan

### Required unit tests

| # | Test | Runs in CI? |
|---|------|-------------|
| 1 | JSON-only round-trip (set → get → delete) | ✅ |
| 2 | `list_providers` accuracy | ✅ |
| 3 | Keyring round-trip | ❌ (`#[ignore]`) |
| 4 | Fallback-on-keyring-failure | ✅ (if simulatable) |
| 5 | Permissions are `0600` on Unix after `set()` | ✅ |

### Manual validation

1. Build on the primary dev platform (Linux/macOS).
2. Run `cargo test -p anie-auth` — all non-ignored tests pass.
3. Run the ignored keyring test manually — verify credentials appear in OS keyring (Keychain Access / `secret-tool lookup`).
4. Build with `--no-default-features` for `anie-auth` — verify it compiles without `keyring`.

---

## Exit criteria

This phase is complete when:

- [ ] `keyring` is in `Cargo.toml` with appropriate platform features.
- [ ] `CredentialStore` compiles and provides `get` / `set` / `delete` / `list_providers`.
- [ ] Conditional compilation works — `--no-default-features` builds clean.
- [ ] JSON fallback works in all unit tests.
- [ ] Existing `AuthStore` / `AuthResolver` code is untouched and all old tests pass.

---

## Follow-on phase

After this phase is green, proceed to:
→ `phase_2_migration_and_resolver.md`
