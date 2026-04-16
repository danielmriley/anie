# Onboarding & Credential Management

**Status**: Implemented in v0.1.0

This document details the new credential storage system using the [`keyring`](https://crates.io/crates/keyring) crate and the improved, menu-driven onboarding experience. The design draws direct inspiration from **pi-mono**'s elegant TUI-first approach (`/login`, `/model`, `/settings` slash commands + interactive provider selection) while adapting it to Rust + `ratatui` + `crossterm`.

## 1. Credential Storage with `keyring`

### Why `keyring` instead of plaintext `~/.anie/auth.json`?
- Uses the **operating system's native encrypted credential store** (no `.env`, no plaintext files by default).
- Matches the security model of professional CLIs (`gh`, `aws`, `doctl`, etc.).
- Automatic fallback for headless environments (servers, Docker, CI).

| Platform       | Backend                          | Visible via                          | Security |
|----------------|----------------------------------|--------------------------------------|----------|
| **macOS**      | Apple Keychain                   | Keychain Access app                  | Touch ID / password |
| **Windows**    | Windows Credential Manager       | Credential Manager                   | OS-level encryption |
| **Linux**      | Linux kernel keyring (`linux-native`) | Kernel keyring tooling / system inspection | Kernel/session scoped |
| **BSD**        | Same as Linux (planned parity)   | Same                                 | Same |

### Adding the dependency (`crates/anie-auth/Cargo.toml`)

```toml
[dependencies]
keyring = { version = "3", features = [
    "apple-native",    # macOS
    "windows-native",  # Windows
    "linux-native",    # Linux build-friendly backend in the current implementation
] }
serde = { version = "1", features = ["derive"] }
```

Make the features optional if you want a lightweight build:

```toml
keyring = { version = "3", features = ["sync-secret-service"], optional = true }
# Then in [features]
default = ["keyring-native"]
keyring-native = ["dep:keyring"]
```

### New `CredentialStore` abstraction (`crates/anie-auth/src/store.rs`)

```rust
use keyring::Entry;
use std::path::PathBuf;

pub struct CredentialStore {
    app_name: String,
    json_fallback: PathBuf, // ~/.anie/auth.json (0600)
}

impl CredentialStore {
    pub fn new() -> Self { /* ... */ }

    pub fn get(&self, provider: &str) -> Option<String> {
        // 1. Try keyring first
        if let Ok(entry) = Entry::new(&self.app_name, provider) {
            if let Ok(key) = entry.get_password() {
                return Some(key);
            }
        }
        // 2. Fallback to existing JSON (for migration)
        // ... load from JSON if present
    }

    pub fn set(&self, provider: &str, key: &str) -> Result<(), Box<dyn std::error::Error>> {
        // Always store in keyring
        let entry = Entry::new(&self.app_name, provider)?;
        entry.set_password(key)?;

        // Optional: keep JSON for portability / headless (still 0600)
        // self.save_to_json(provider, key)?;
        Ok(())
    }

    pub fn delete(&self, provider: &str) { /* ... */ }
}
```

**Migration path** (one-time on first load):
- If `auth.json` exists → import every key into keyring → optionally delete or mark read-only.
- Log a friendly message: `"✅ Migrated credentials to OS keyring (old file preserved for now)"`.

### CLI / Config integration
- `anie config` and the new `anie onboard` command now use `CredentialStore`.
- `--api-key` flag still overrides (for one-off sessions).
- Env vars (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, etc.) remain supported for quick testing.

## 2. Improved Onboarding Experience

### Philosophy (inspired by pi-mono)
- **First-run** should feel magical and zero-friction (like `pi` after `npm install`).
- **Menu-driven TUI** instead of plain text prompts.
- Re-runnable anytime via `anie onboard` or the slash command `/onboard` inside the main agent TUI.
- Detect local servers automatically (Ollama, LM Studio, etc.) — exactly as you already do.
- Support both **API keys** and future **OAuth** flows.

### Triggering Onboarding
1. **Automatic first-run** (if no providers configured):
   - On `anie` start → check `CredentialStore` + config.
   - If empty → show full-screen onboarding TUI immediately.
2. **Manual**:
   - `anie onboard` (new subcommand)
   - Inside running agent TUI: type `/onboard`

### TUI Menu Flow (ratatui-based)

The onboarding lives in a new `OnboardingScreen` widget inside `crates/anie-tui`.

**Main Menu** (inspired by pi-mono's `/login` + `/settings`):
```
╔══════════════════════════════════════════════════════════════╗
║                  Welcome to Anie — First Run                 ║
╟──────────────────────────────────────────────────────────────╢
║ 1. Configure Local Server (Ollama / LM Studio detected)     ║
║ 2. Add API Key Provider                                      ║
║ 3. List & Manage Existing Providers                          ║
║ 4. Advanced / Custom OpenAI-compatible endpoint              ║
║                                                              ║
║  [↑↓] Navigate   [Enter] Select   [q] Quit                   ║
╚══════════════════════════════════════════════════════════════╝
```

**Sub-flows** (all keyboard-driven, no mouse):

- **Local Server** → Auto-detect → "Use Ollama on http://localhost:11434? (Y/n)" → test connection → save as default provider.
- **Add API Key Provider** → List of popular presets:
  ```
  • OpenAI (gpt-4o, o1, etc.)
  • Anthropic (Claude 3.5/4)
  • xAI / Grok
  • Groq
  • Together.ai
  • Fireworks
  • Mistral
  • Custom (base URL + model list)
  ```
  → Prompt for API key (masked input) → `CredentialStore::set()` → success toast.
- **Manage Existing** → Table of providers + "Test connection" + "Delete" buttons.
- **Custom** → Form fields for `base_url`, `api_key`, `default_model`.

**Visual polish** (use existing `anie-tui` components):
- Use `ratatui::widgets::List` + `Table` for menus.
- `crossterm` event loop with timeout for responsive feel.
- Green success / red error banners (like pi-mono's footer status).
- Progress spinner while testing a provider connection.
- Keyboard shortcuts shown at bottom (exactly like pi-mono's footer).

### New Commands
- `anie onboard` → launches the full-screen TUI menu.
- Inside agent TUI: `/onboard` → same screen (modal overlay).
- `/providers` → quick list + test (future extension).

## 3. Implementation Checklist

- [x] Add `keyring` dependency + features.
- [x] Implement `CredentialStore` + migration logic in `anie-auth`.
- [x] Add `OnboardingScreen` to `anie-tui`.
- [x] Wire `anie onboard` subcommand in `anie-cli`.
- [x] Update first-run logic in main entrypoint.
- [x] Add integration and unit coverage for credential round-trip and migration.
- [x] Update README with the `anie onboard`, `/onboard`, and `/providers` flow.
- [x] Document in `docs/arch/` how providers now resolve (keyring → JSON fallback → env).

## 4. Implementation Deviations

A few implementation details differ from the original proposal:

- **Linux backend**: the current build uses `keyring`'s `linux-native` backend instead of Secret Service so the workspace can build and test without external `dbus-1` development packages.
- **JSON compatibility mirror**: after successful native keyring writes, credentials are also mirrored into `~/.anie/auth.json` to preserve compatibility for provider enumeration and headless fallback.
- **Config writes**: onboarding and provider-management changes are written through `anie-config::ConfigMutator` (`toml_edit`) so existing user comments and formatting are preserved.
- **Provider management timing**: `/providers` is no longer just a future extension; it is implemented as a TUI overlay and is also reachable from the onboarding main menu when existing providers are present.
- **README media**: the README was updated textually; no GIF asset was added in this implementation pass.

## 5. Future Extensions

## 4. Future Extensions
- OAuth support (Azure, Google Vertex, etc.) can slot into the same menu.
- "Save as default model" after successful test.
- Telemetry opt-in during onboarding (anonymous).

