# Phase 2 — Migration and Resolver Update

This phase migrates existing JSON-file credentials into the OS keyring and updates `AuthResolver` to use the new `CredentialStore` as its primary backend.

## Why this phase exists

Phase 1 introduced `CredentialStore` alongside the old JSON-only code.  Now the resolver and all credential consumers need to switch to the new abstraction.  Existing users who already have `~/.anie/auth.json` credentials must be migrated transparently.

---

## Files expected to change

### Primary

- `crates/anie-auth/src/store.rs` — add `migrate_from_json()` method.
- `crates/anie-auth/src/lib.rs` — update `AuthResolver` to use `CredentialStore`; deprecate or remove standalone `save_api_key()` / `load_auth_store()`.

### Secondary

- `crates/anie-cli/src/onboarding.rs` — switch `save_api_key()` calls to `CredentialStore::set()`.
- `crates/anie-cli/src/lib.rs` — initialize `CredentialStore` early and pass into `AuthResolver`.
- `crates/anie-cli/src/controller.rs` — update `AuthResolver` construction if it currently calls `AuthResolver::new()` directly.

### No changes yet

- `crates/anie-tui/` — no TUI work yet.
- `crates/anie-config/` — no config schema changes.

---

## Sub-steps

### Sub-step A — Implement `migrate_from_json()`

Add to `CredentialStore`:

```rust
/// One-time migration: import all keys from `auth.json` into the keyring.
/// Returns the number of credentials migrated.
pub fn migrate_from_json(&self) -> Result<usize>;
```

Implementation:

1. Load the JSON file using existing `load_auth_store_from()`.
2. For each `(provider, AuthCredential::ApiKey { key })` entry:
   - Call `self.set(provider, &key)`.
   - Track success count.
3. If all succeed, rename `auth.json` → `auth.json.migrated` (preserve as backup, do not delete).
4. Log: `"✅ Migrated {n} credential(s) to OS keyring (old file preserved as auth.json.migrated)"`.
5. If keyring writes fail, leave `auth.json` untouched and log a warning — the JSON fallback in `get()` will continue to work.

### Sub-step B — Update `AuthResolver` to use `CredentialStore`

Modify `AuthResolver`:

```rust
pub struct AuthResolver {
    pub cli_api_key: Option<String>,
    pub config: AnieConfig,
    credential_store: CredentialStore,
}
```

Update `AuthResolver::new()`:

```rust
pub fn new(cli_api_key: Option<String>, config: AnieConfig) -> Self {
    Self {
        cli_api_key,
        config,
        credential_store: CredentialStore::new(),
    }
}
```

Add a test constructor:

```rust
pub fn with_credential_store(
    cli_api_key: Option<String>,
    config: AnieConfig,
    credential_store: CredentialStore,
) -> Self;
```

Update the `RequestOptionsResolver::resolve()` implementation:

```rust
// Resolution order:
// 1. CLI --api-key flag
// 2. CredentialStore (keyring → JSON fallback)
// 3. Environment variable (configured or builtin)
```

Replace the current `load_auth_store_from()` call with `self.credential_store.get(&model.provider)`.

Remove `auth_path` field and `with_auth_path()` — the `CredentialStore` already handles path configuration internally.  For tests, use `with_credential_store()` instead.

### Sub-step C — Deprecate standalone JSON functions

Mark the following as `#[deprecated]` with a message pointing to `CredentialStore`:

- `save_api_key()`
- `save_api_key_to()`
- `load_auth_store()`
- `load_auth_store_from()`
- `auth_file_path()`

Do not remove them yet — external tests or scripts may reference them.  They will be removed in a future cleanup.

### Sub-step D — Trigger migration on startup

In `crates/anie-cli/src/lib.rs`, add migration logic before entering any mode:

```rust
pub async fn run(cli: Cli) -> Result<()> {
    init_tracing();
    // ...

    let credential_store = CredentialStore::new();
    if credential_store.should_migrate() {
        match credential_store.migrate_from_json() {
            Ok(n) if n > 0 => tracing::info!("migrated {n} credential(s) to OS keyring"),
            Ok(_) => {}
            Err(e) => tracing::warn!("credential migration skipped: {e}"),
        }
    }

    // ... rest of startup
}
```

Add `should_migrate()` to `CredentialStore`:

```rust
/// Returns true if `auth.json` exists and `auth.json.migrated` does not.
pub fn should_migrate(&self) -> bool;
```

### Sub-step E — Update onboarding to use `CredentialStore`

In `crates/anie-cli/src/onboarding.rs`:

- Replace `save_api_key(provider, &key)?` with `CredentialStore::new().set(provider, &key)?`.
- Remove the `use anie_auth::save_api_key` import.

### Sub-step F — Update existing tests

- **`AuthResolver` tests** in `crates/anie-auth/src/lib.rs`:
  - Update to use `AuthResolver::with_credential_store(...)` with a test `CredentialStore` pointing at a tempdir.
  - Priority chain test: CLI → keyring/JSON → env var.
  - Local model without key still resolves to `None`.

- **Migration tests** in `crates/anie-auth/src/store.rs`:
  - Create a JSON file with two providers → call `migrate_from_json()` → verify both are accessible via `get()`.
  - Verify `auth.json` is renamed to `auth.json.migrated`.
  - Verify `should_migrate()` returns `false` after migration.
  - Verify idempotency: calling `migrate_from_json()` again is a no-op.

---

## Constraints

1. **Migration must be non-destructive.**  The original `auth.json` is renamed, never deleted.
2. **Migration failures must not block startup.**  If the keyring is unavailable, the JSON fallback continues to work transparently.
3. **The `--api-key` CLI flag continues to override everything.**
4. **Environment variables continue to work** as a tertiary source.
5. **Do not touch TUI code** — that is Phase 3.

---

## Test plan

### Required unit tests

| # | Test | Crate |
|---|------|-------|
| 1 | Resolver priority: CLI → credential store → env | `anie-auth` |
| 2 | Resolver allows local models without keys | `anie-auth` |
| 3 | Migration imports all JSON keys | `anie-auth` |
| 4 | Migration renames JSON to `.migrated` | `anie-auth` |
| 5 | `should_migrate()` returns false after migration | `anie-auth` |
| 6 | Migration is idempotent | `anie-auth` |
| 7 | Onboarding saves via `CredentialStore` | `anie-cli` |

### Manual validation

1. Place a test `auth.json` in `~/.anie/` with a dummy key.
2. Run `anie` — verify migration log message appears.
3. Verify `~/.anie/auth.json.migrated` exists.
4. Verify the key is retrievable from OS keyring (`secret-tool lookup` on Linux, Keychain Access on macOS).
5. Run `anie` again — verify migration does not re-trigger.
6. Delete `~/.anie/auth.json.migrated` and restore `auth.json` — verify migration re-triggers.

---

## Risks

1. **Different `AuthResolver` constructors across call sites** — search for all `AuthResolver::new()` calls and update them.
2. **Test isolation** — tests that previously used `with_auth_path()` need updating to `with_credential_store()`.
3. **Backward compatibility** — deprecated functions still compile; nothing breaks immediately.

---

## Exit criteria

This phase is complete when:

- [ ] `AuthResolver` uses `CredentialStore` internally.
- [ ] JSON → keyring migration runs on startup when applicable.
- [ ] Migration is non-destructive (rename, not delete).
- [ ] Onboarding writes credentials via `CredentialStore`.
- [ ] All existing auth tests pass with updated constructors.
- [ ] Migration-specific tests pass.
- [ ] `--api-key` CLI override still works.
- [ ] Environment variable fallback still works.

---

## Follow-on phase

After this phase is green, proceed to:
→ `phase_3_onboarding_tui.md`
