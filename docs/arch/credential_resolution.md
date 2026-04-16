# Credential Resolution Order

`anie` resolves request credentials in the following order:

1. `--api-key` CLI flag
2. native OS keyring storage via `anie-auth::CredentialStore`
3. JSON compatibility files:
   - `~/.anie/auth.json`
   - `~/.anie/auth.json.migrated`
4. configured environment variables (`api_key_env` in provider config)
5. built-in environment variable defaults such as `OPENAI_API_KEY` and `ANTHROPIC_API_KEY`

## Native keyring backends

The current implementation uses the `keyring` crate with these platform backends:

- macOS: `apple-native`
- Windows: `windows-native`
- Linux: `linux-native`

### Why Linux uses `linux-native`

The original design proposed Secret Service / libsecret support (`sync-secret-service`), but the current workspace build environment does not provide the required `dbus-1` development packages. To keep `cargo build` and `cargo test` green without external system dependencies, the Linux build currently targets the kernel keyring backend.

## Migration from legacy `auth.json`

On startup, `anie-cli` asks `CredentialStore` whether legacy plaintext credentials should be migrated.

Migration rules:

- if `~/.anie/auth.json` exists
- and the native keyring backend is enabled
- and `~/.anie/auth.json.migrated` does not already exist
- and the file contains at least one credential

then `CredentialStore::migrate_from_json()`:

1. imports each provider credential into the native keyring
2. renames `auth.json` to `auth.json.migrated`
3. logs the migration event

If native keyring storage is unavailable, migration is skipped and the JSON file remains the active fallback source.

## JSON fallback behavior

`CredentialStore::set()` prefers the native keyring. When that write succeeds and a fallback path is available, it also mirrors the credential into `~/.anie/auth.json` for compatibility. If the native write fails, it falls back to writing JSON only.

`CredentialStore::get()` tries the native keyring first, then checks the current JSON compatibility file, then the migrated backup file.

This preserves compatibility for:

- headless environments
- CI builds without keyring support
- users migrating from older plaintext-only releases
