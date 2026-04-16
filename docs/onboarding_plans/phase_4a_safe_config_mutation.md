# Phase 4a — Safe Config Mutation

This phase adds safe, non-destructive configuration writing to the `anie-config` crate using `toml_edit`.

## Why this phase exists

Phases 4 and 5 allow the user to add, edit, and delete providers from inside the running TUI (via `/onboard` and `/providers`). 

Currently, `anie-config` only supports deserializing `AnieConfig` from a file. If we simply serialize the struct back to disk using `serde` and `toml::to_string()`, it will **destroy all user comments, structural formatting, and commented-out templates** in `~/.anie/config.toml`.

To safely mutate the configuration without data loss, we must parse it as a document-preserving syntax tree (`toml_edit::DocumentMut`), mutate only the specific provider/model tables, and write it back.

---

## Files expected to change

### Primary
- `crates/anie-config/Cargo.toml` — add `toml_edit` dependency.
- `crates/anie-config/src/mutation.rs` — new file containing safe mutation helpers.
- `crates/anie-config/src/lib.rs` — expose the mutation API.

### Secondary
- `crates/anie-cli/src/onboarding.rs` — update to use the safe mutation API instead of string templates.
- `crates/anie-tui/src/providers.rs` — use the mutation API for deletions and default changes.

---

## Sub-steps

### Sub-step A — Add `toml_edit` dependency
In `crates/anie-config/Cargo.toml`:
```toml
[dependencies]
toml_edit = "0.22"
```

### Sub-step B — Build the ConfigMutator abstraction
Create `crates/anie-config/src/mutation.rs`:

```rust
use anyhow::{Context, Result};
use toml_edit::{DocumentMut, value};
use std::fs;
use std::path::Path;

pub struct ConfigMutator {
    doc: DocumentMut,
    path: std::path::PathBuf,
}

impl ConfigMutator {
    /// Load the config file while preserving formatting and comments.
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let doc = content.parse::<DocumentMut>()?;
        Ok(Self { doc, path: path.to_path_buf() })
    }

    /// Set the default model and provider.
    pub fn set_default_model(&mut self, provider: &str, model_id: &str) {
        let model_table = self.doc["model"].or_insert(toml_edit::table());
        model_table["provider"] = value(provider);
        model_table["id"] = value(model_id);
    }

    /// Add or update a provider's configuration.
    pub fn upsert_provider(&mut self, name: &str, base_url: Option<&str>, api: Option<&str>) {
        let providers = self.doc["providers"].or_insert(toml_edit::table());
        let provider_table = providers[name].or_insert(toml_edit::table());
        
        if let Some(url) = base_url {
            provider_table["base_url"] = value(url);
        }
        if let Some(api_kind) = api {
            provider_table["api"] = value(api_kind);
        }
    }

    /// Remove a provider entirely.
    pub fn remove_provider(&mut self, name: &str) {
        if let Some(providers) = self.doc.get_mut("providers") {
            if let Some(table) = providers.as_table_mut() {
                table.remove(name);
            }
        }
    }

    /// Save the preserved document back to disk.
    pub fn save(&self) -> Result<()> {
        fs::write(&self.path, self.doc.to_string())?;
        Ok(())
    }
}
```

### Sub-step C — Write unit tests for mutator
1. Load a mock TOML with comments.
2. Call `upsert_provider()`.
3. Call `save()` to a string.
4. Assert that the original comments still exist and the new provider was added.
5. Verify `remove_provider()` cleanly drops the provider block without affecting `[context]` or `[compaction]` blocks.

### Sub-step D — Integrate into CLI and TUI actions
When Phase 4 and Phase 5 need to save changes:
```rust
let mut mutator = ConfigMutator::load(&config_path).unwrap_or_default();
mutator.upsert_provider("custom_local", Some("http://localhost:8080/v1"), None);
mutator.set_default_model("custom_local", "llama-3");
mutator.save()?;
```

---

## Follow-on phase

After this phase is green, proceed to:
→ `phase_4_cli_wiring.md`