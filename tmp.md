# Dynamic Model Selection Menus

**Status**: Proposed for v0.1.1 (post-onboarding & keyring)

This document outlines how to add **context-aware model menus** to the onboarding flow and TUI commands, exactly like **pi-mono** does with its `/model` and provider selection screens.  

The goal: when a user picks a provider (local or API), Anie immediately shows a navigable list of available models and lets them select one — no more typing model IDs manually or guessing what’s supported.

## 1. Why This Matters (pi-mono Inspiration)

pi-mono’s strength is its **zero-friction model picker**:
- `/login` or first-run shows providers.
- Selecting a provider instantly lists every model the backend advertises (Ollama pulls from `/api/tags`, OpenAI pulls from `/v1/models`, etc.).
- Arrow keys + Enter to pick → instantly becomes the active model.
- Same list appears on `/model` or when switching providers mid-session.

We already have the perfect foundation:
- `anie-provider` trait + built-in implementations.
- Menu-driven `OnboardingScreen` (just shipped).
- Slash-command system in `anie-tui`.

This change will make onboarding feel *magical* and every `/model` or provider switch feel professional.

## 2. New Provider Trait Extension

Update `crates/anie-provider/src/lib.rs`:

```rust
#[async_trait]
pub trait Provider: Send + Sync + Debug {
    // ... existing methods ...

    /// List all available models for this provider (with metadata).
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Optional: test connection + fetch models in one go (used heavily in onboarding).
    async fn test_and_list_models(&self) -> Result<(bool, Vec<ModelInfo>), ProviderError> {
        let models = self.list_models().await?;
        Ok((true, models)) // or do a real /health check first
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,           // human-friendly display name
    pub context_length: Option<u64>,
    pub supports_images: bool,
    pub supports_reasoning: bool,
    // add more fields as needed (price, speed tier, etc.)
}
```

### Built-in Provider Updates (`crates/anie-providers-builtin`)

- **Ollama / LM Studio**: call `/api/tags` or equivalent and map to `ModelInfo`.
- **OpenAI-compatible** (including Groq, Together, Fireworks, xAI): call `/v1/models`.
- **Anthropic**: use their `/v1/models` endpoint (they expose it).
- Cache the list per-provider for 5-10 minutes (simple in-memory + `tokio::time`).

Add a new `ModelListCache` helper in `anie-auth` or a tiny new `anie-model-cache` crate if it grows.

## 3. Onboarding Flow Updates (`crates/anie-tui/src/onboarding.rs`)

Extend the existing `OnboardingScreen`:

```rust
// After user selects a provider (local or API key)
async fn show_model_picker(
    &mut self,
    provider: &dyn Provider,
) -> Result<Option<String>, TuiError> {
    let models = provider.list_models().await?;

    let list_widget = List::new(
        models.iter().map(|m| ListItem::new(format!("{} — {}", m.name, m.id)))
    );

    // Standard ratatui list with highlight + Enter to select
    // Footer: "↑↓ Navigate  Enter Select  / Filter  q Back"

    // On selection → save to CredentialStore + config.toml as default model
    // Show success toast: "✅ Switched to qwen3:32b"
    Ok(Some(selected_id))
}
```

**Onboarding main menu now becomes**:
1. Local Server (Ollama/LM Studio) → auto-detect → **model list**
2. Add API Provider → pick preset → enter key → **model list**
3. Custom endpoint → base URL + key → **model list**
4. Manage existing providers → table with “Test + Pick Model” button

Add a **“Refresh models”** button (useful for Ollama after pulling new tags).

## 4. TUI Slash Commands & Context Menus

Inside the main agent TUI (`crates/anie-tui/src/app.rs`):

- `/model` → opens full-screen model picker for **current provider** (or lets you switch provider first).
- `/providers` → existing list becomes a table where each row has a “View Models” action.
- When switching providers mid-session (future `/provider <name>`), immediately show the model picker.

Reuse the same `ModelPickerWidget` component everywhere — keep it DRY.

Add keyboard shortcut hint in the main footer:
`[ /model ] Pick model   [ /providers ] Manage providers`

## 5. Config & Persistence

- When a model is chosen, write it to `~/.anie/config.toml` under the provider block (exactly like you already do for default model).
- Support per-project overrides via `.anie/config.toml` in the workspace root.
- `anie config` CLI command gets a new `--models` subcommand that prints the current list.

## 6. Implementation Checklist

- [ ] Extend `Provider` trait with `list_models` + `ModelInfo`.
- [ ] Implement for all built-in providers (Ollama, OpenAI-compat, Anthropic).
- [ ] Add `ModelPickerWidget` reusable component in `anie-tui`.
- [ ] Update `OnboardingScreen` to call it after every provider selection.
- [ ] Wire `/model` and enhance `/providers` in main TUI.
- [ ] Add simple in-memory cache + refresh logic.
- [ ] Update integration tests (mock provider that returns fake model list).
- [ ] Refresh README + `docs/onboarding-and-keyring.md` with new screenshots/GIFs.
- [ ] Update `docs/status_report_*.md` and milestone checklist.

