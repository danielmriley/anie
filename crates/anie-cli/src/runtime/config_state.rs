//! `ConfigState` — configured runtime knobs owned by the controller.
//!
//! Bundles the static config, persisted runtime defaults, and the
//! currently-selected model / thinking state so controller code can
//! delegate config-focused mutations instead of touching five bare
//! fields.

use anyhow::{Result, anyhow};

use anie_config::{AnieConfig, CliOverrides, load_config};
use anie_provider::{ApiKind, Model, ThinkingLevel};
use anie_session::SessionContext;

use crate::{
    model_catalog::{
        build_model_catalog, fallback_model_from_provider, resolve_model, upsert_model,
    },
    runtime_state::{RuntimeState, save_runtime_state},
};

/// Result of reloading config and recomputing the active selection.
pub(crate) struct ReloadOutcome {
    pub(crate) model_catalog: Vec<Model>,
    pub(crate) current_model: Model,
    pub(crate) current_thinking: ThinkingLevel,
}

/// Config + runtime-state handle for the controller.
pub(crate) struct ConfigState {
    anie_config: AnieConfig,
    runtime_state: RuntimeState,
    current_model: Model,
    current_thinking: ThinkingLevel,
    cli_api_key: Option<String>,
    #[cfg(test)]
    runtime_state_path_override: Option<std::path::PathBuf>,
}

impl ConfigState {
    pub(crate) fn new(
        anie_config: AnieConfig,
        runtime_state: RuntimeState,
        current_model: Model,
        current_thinking: ThinkingLevel,
        cli_api_key: Option<String>,
    ) -> Self {
        Self {
            anie_config,
            runtime_state,
            current_model,
            current_thinking,
            cli_api_key,
            // In test builds, persistence diverts to a scratch file
            // BY DEFAULT. Any test that drives an action which
            // persists (SetModel, SetThinking, session switches)
            // through a ConfigState left at the real default was
            // overwriting the developer's actual ~/.anie/state.json
            // on every `cargo test` run — observed clobbering the
            // selected model and the num_ctx overrides (2026-06-12).
            // Tests that assert on persistence set their own
            // explicit path via set_runtime_state_path_for_test.
            #[cfg(test)]
            runtime_state_path_override: Some(std::env::temp_dir().join(format!(
                "anie-test-runtime-state-{}.json",
                std::process::id()
            ))),
        }
    }

    pub(crate) fn anie_config(&self) -> &AnieConfig {
        &self.anie_config
    }

    pub(crate) fn current_model(&self) -> &Model {
        &self.current_model
    }

    pub(crate) fn current_thinking(&self) -> ThinkingLevel {
        self.current_thinking
    }

    pub(crate) fn cli_api_key(&self) -> Option<&str> {
        self.cli_api_key.as_deref()
    }

    pub(crate) fn set_model(&mut self, model: Model) {
        self.current_model = model;
    }

    pub(crate) fn set_thinking(&mut self, thinking: ThinkingLevel) {
        self.current_thinking = thinking;
    }

    pub(crate) fn active_ollama_num_ctx_override(&self) -> Option<u64> {
        if self.current_model.api != ApiKind::OllamaChatApi {
            return None;
        }
        self.runtime_state
            .ollama_num_ctx_overrides
            .get(&ollama_num_ctx_key(&self.current_model))
            .copied()
            .or_else(|| {
                default_num_ctx_clamp(
                    self.current_model.context_window,
                    std::env::var("ANIE_DEFAULT_NUM_CTX").ok().as_deref(),
                )
            })
    }

    pub(crate) fn effective_ollama_context_window(&self) -> u64 {
        self.active_ollama_num_ctx_override()
            .unwrap_or(self.current_model.context_window)
    }

    pub(crate) fn set_ollama_num_ctx_override(&mut self, value: u64) {
        self.runtime_state
            .ollama_num_ctx_overrides
            .insert(ollama_num_ctx_key(&self.current_model), value);
    }

    pub(crate) fn clear_ollama_num_ctx_override(&mut self) {
        self.runtime_state
            .ollama_num_ctx_overrides
            .remove(&ollama_num_ctx_key(&self.current_model));
    }

    pub(crate) fn apply_session_overrides(
        &mut self,
        session_context: &SessionContext,
        model_catalog: &mut Vec<Model>,
    ) {
        if let Some((provider, model_id)) = &session_context.model
            && let Some(model) = model_catalog
                .iter()
                .find(|candidate| candidate.provider == *provider && candidate.id == *model_id)
                .cloned()
                .or_else(|| {
                    fallback_model_from_provider(
                        provider,
                        model_id,
                        &self.anie_config,
                        model_catalog,
                        &anie_auth::CredentialStore::new(),
                    )
                })
        {
            upsert_model(model_catalog, &model);
            self.current_model = model;
        }
        if let Some(thinking) = session_context.thinking_level {
            self.current_thinking = thinking;
        }
    }

    pub(crate) fn persist_runtime_state(&mut self, session_id: &str) -> Result<()> {
        self.runtime_state.provider = Some(self.current_model.provider.clone());
        self.runtime_state.model = Some(self.current_model.id.clone());
        self.runtime_state.thinking = Some(self.current_thinking);
        self.runtime_state.last_session_id = Some(session_id.to_string());
        #[cfg(test)]
        if let Some(path) = &self.runtime_state_path_override {
            return crate::runtime_state::save_runtime_state_to(path, &self.runtime_state);
        }
        save_runtime_state(&self.runtime_state)
    }

    pub(crate) async fn reload_from_disk(
        &mut self,
        requested_provider: Option<&str>,
        requested_model: Option<&str>,
    ) -> Result<ReloadOutcome> {
        let config = load_config(CliOverrides::default())?;
        let (model_catalog, local_models_available) = build_model_catalog(&config).await;
        self.apply_reloaded_config(
            config,
            model_catalog,
            local_models_available,
            requested_provider,
            requested_model,
        )
    }

    fn apply_reloaded_config(
        &mut self,
        config: AnieConfig,
        model_catalog: Vec<Model>,
        local_models_available: bool,
        requested_provider: Option<&str>,
        requested_model: Option<&str>,
    ) -> Result<ReloadOutcome> {
        let current_provider = requested_provider.unwrap_or(&self.current_model.provider);
        let current_model = requested_model.unwrap_or(&self.current_model.id);
        let selected_model = resolve_model(
            Some(current_provider),
            Some(current_model),
            &model_catalog,
            local_models_available,
        )
        .or_else(|_| {
            fallback_model_from_provider(
                current_provider,
                current_model,
                &config,
                &model_catalog,
                &anie_auth::CredentialStore::new(),
            )
            .ok_or_else(|| anyhow!("no model named '{current_model}' was found"))
        })
        .or_else(|_| {
            resolve_model(
                Some(&config.model.provider),
                Some(&config.model.id),
                &model_catalog,
                local_models_available,
            )
        })
        .or_else(|_| {
            fallback_model_from_provider(
                &config.model.provider,
                &config.model.id,
                &config,
                &model_catalog,
                &anie_auth::CredentialStore::new(),
            )
            .ok_or_else(|| anyhow!("no model named '{}' was found", config.model.id))
        })?;

        self.anie_config = config;
        self.current_model = selected_model.clone();

        Ok(ReloadOutcome {
            model_catalog,
            current_model: selected_model,
            current_thinking: self.current_thinking,
        })
    }

    #[cfg(test)]
    fn persist_runtime_state_to(&mut self, path: &std::path::Path, session_id: &str) -> Result<()> {
        self.runtime_state.provider = Some(self.current_model.provider.clone());
        self.runtime_state.model = Some(self.current_model.id.clone());
        self.runtime_state.thinking = Some(self.current_thinking);
        self.runtime_state.last_session_id = Some(session_id.to_string());
        crate::runtime_state::save_runtime_state_to(path, &self.runtime_state)
    }

    #[cfg(test)]
    pub(crate) fn set_runtime_state_path_for_test(&mut self, path: std::path::PathBuf) {
        self.runtime_state_path_override = Some(path);
    }
}

fn ollama_num_ctx_key(model: &Model) -> String {
    format!("{}:{}", model.provider, model.id)
}

/// Default `num_ctx` ceiling for Ollama models whose advertised
/// context window exceeds what consumer hardware can serve. PR19 of
/// `docs/local_model_augmentation/` (field notes,
/// 2026-06-12_gemma4_baseline.md): forwarding a discovered 131k
/// window as `num_ctx` kills llama-server outright on a 6GB-VRAM
/// machine (`GGML_ASSERT(n_inputs < GGML_SCHED_MAX_SPLIT_INPUTS)`),
/// and even where it survives, the KV-cache allocation is gigabytes
/// the rlm ceiling (16,384 by default) has already made unnecessary.
///
/// An explicit `/context-length` override always wins (checked
/// before this in `active_ollama_num_ctx_override`). Tune or
/// effectively disable via `ANIE_DEFAULT_NUM_CTX` (set it at or
/// above the model's window to pass the full window through). The
/// clamp feeds `effective_ollama_context_window`, so plan 04's
/// `PromptTier` sees the serveable window, not the advertised one —
/// a freshly-pulled long-context model lands in the Small tier on
/// small hardware instead of masquerading as a 131k-window giant.
const DEFAULT_NUM_CTX_CAP: u64 = 32_768;

fn default_num_ctx_clamp(context_window: u64, env_cap: Option<&str>) -> Option<u64> {
    let cap = env_cap
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|cap| *cap > 0)
        .unwrap_or(DEFAULT_NUM_CTX_CAP);
    (context_window > cap).then_some(cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_state::load_runtime_state_from;
    use anie_provider::{CostPerMillion, ModelCompat};

    fn model(id: &str, provider: &str) -> Model {
        model_with_api(id, provider, ApiKind::OpenAICompletions)
    }

    fn model_with_api(id: &str, provider: &str, api: ApiKind) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            provider: provider.into(),
            api,
            base_url: "http://localhost:11434/v1".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    #[test]
    fn persist_runtime_state_writes_expected_fields() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("state.json");
        let mut state = ConfigState::new(
            AnieConfig::default(),
            RuntimeState::default(),
            model("qwen3:32b", "ollama"),
            ThinkingLevel::High,
            None,
        );

        state
            .persist_runtime_state_to(&path, "session-123")
            .expect("persist runtime state");
        let saved = load_runtime_state_from(&path).expect("load runtime state");

        assert_eq!(saved.provider.as_deref(), Some("ollama"));
        assert_eq!(saved.model.as_deref(), Some("qwen3:32b"));
        assert_eq!(saved.thinking, Some(ThinkingLevel::High));
        assert_eq!(saved.last_session_id.as_deref(), Some("session-123"));
    }

    #[test]
    fn persist_runtime_state_to_returns_write_errors() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut state = ConfigState::new(
            AnieConfig::default(),
            RuntimeState::default(),
            model("qwen3:32b", "ollama"),
            ThinkingLevel::High,
            None,
        );

        let error = state
            .persist_runtime_state_to(tempdir.path(), "session-123")
            .expect_err("directory path cannot be written as runtime state file");

        assert!(
            error.to_string().contains("failed to write"),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn apply_session_overrides_updates_current_model_and_thinking() {
        let mut state = ConfigState::new(
            AnieConfig::default(),
            RuntimeState::default(),
            model("gpt-4o", "openai"),
            ThinkingLevel::Low,
            None,
        );
        let mut catalog = vec![model("gpt-4o", "openai"), model("qwen3:32b", "ollama")];
        let mut session_context = SessionContext::empty();
        session_context.model = Some(("ollama".into(), "qwen3:32b".into()));
        session_context.thinking_level = Some(ThinkingLevel::High);

        state.apply_session_overrides(&session_context, &mut catalog);

        assert_eq!(state.current_model().provider, "ollama");
        assert_eq!(state.current_model().id, "qwen3:32b");
        assert_eq!(state.current_thinking(), ThinkingLevel::High);
    }

    #[test]
    fn switching_to_non_thinking_model_preserves_user_thinking_preference() {
        // PR 5 invariant. The user's thinking level is a
        // preference applied across model switches, not a
        // per-model setting. Flipping the active model to a
        // non-thinking one (e.g. gemma3:1b) must NOT reset the
        // preference — it silently drops at request build, and
        // switching back re-applies it automatically.
        let mut state = ConfigState::new(
            AnieConfig::default(),
            RuntimeState::default(),
            model("qwen3:32b", "ollama"),
            ThinkingLevel::Medium,
            None,
        );
        assert_eq!(state.current_thinking(), ThinkingLevel::Medium);

        // Switch to a non-thinking model.
        state.set_model(model("gemma3:1b", "ollama"));
        assert_eq!(
            state.current_thinking(),
            ThinkingLevel::Medium,
            "thinking level must not be reset when switching to a non-thinking model"
        );

        // Switch back — thinking still Medium.
        state.set_model(model("qwen3:32b", "ollama"));
        assert_eq!(state.current_thinking(), ThinkingLevel::Medium);
    }

    #[test]
    fn reload_from_disk_swaps_model_without_changing_thinking() {
        let mut state = ConfigState::new(
            AnieConfig::default(),
            RuntimeState::default(),
            model("gpt-4o", "openai"),
            ThinkingLevel::Medium,
            None,
        );
        let mut config = AnieConfig::default();
        config.model.provider = "ollama".into();
        config.model.id = "qwen3:32b".into();
        let catalog = vec![model("gpt-4o", "openai"), model("qwen3:32b", "ollama")];

        let outcome = state
            .apply_reloaded_config(config, catalog, true, Some("ollama"), Some("qwen3:32b"))
            .expect("apply reloaded config");

        assert_eq!(outcome.current_model.provider, "ollama");
        assert_eq!(outcome.current_model.id, "qwen3:32b");
        assert_eq!(outcome.current_thinking, ThinkingLevel::Medium);
        assert_eq!(state.current_model().provider, "ollama");
        assert_eq!(state.current_thinking(), ThinkingLevel::Medium);
    }

    #[test]
    fn active_ollama_num_ctx_override_returns_none_when_no_runtime_entry() {
        let state = ConfigState::new(
            AnieConfig::default(),
            RuntimeState::default(),
            model_with_api("qwen3:32b", "ollama", ApiKind::OllamaChatApi),
            ThinkingLevel::Off,
            None,
        );

        assert_eq!(state.active_ollama_num_ctx_override(), None);
    }

    /// PR19 (field notes 2026-06-12_gemma4_baseline.md): a freshly
    /// pulled long-context model must not forward its advertised
    /// window as num_ctx — a discovered 131k window killed
    /// llama-server outright on a 6GB-VRAM machine.
    #[test]
    fn discovered_context_window_above_cap_is_clamped_by_default() {
        let mut model = model_with_api("gemma4:e4b", "ollama", ApiKind::OllamaChatApi);
        model.context_window = 131_072;
        let state = ConfigState::new(
            AnieConfig::default(),
            RuntimeState::default(),
            model,
            ThinkingLevel::Off,
            None,
        );
        assert_eq!(state.active_ollama_num_ctx_override(), Some(32_768));
        // The clamp feeds the effective window, so PromptTier sees
        // the serveable size, not the advertised one.
        assert_eq!(state.effective_ollama_context_window(), 32_768);
    }

    #[test]
    fn explicit_context_length_override_beats_the_default_clamp() {
        let mut runtime_state = RuntimeState::default();
        runtime_state
            .ollama_num_ctx_overrides
            .insert("ollama:gemma4:e4b".into(), 65_536);
        let mut model = model_with_api("gemma4:e4b", "ollama", ApiKind::OllamaChatApi);
        model.context_window = 131_072;
        let state = ConfigState::new(
            AnieConfig::default(),
            runtime_state,
            model,
            ThinkingLevel::Off,
            None,
        );
        assert_eq!(state.active_ollama_num_ctx_override(), Some(65_536));
    }

    #[test]
    fn windows_at_or_below_cap_pass_through_unclamped() {
        assert_eq!(default_num_ctx_clamp(32_768, None), None);
        assert_eq!(default_num_ctx_clamp(8_192, None), None);
        assert_eq!(default_num_ctx_clamp(32_769, None), Some(32_768));
    }

    #[test]
    fn non_ollama_models_are_never_clamped() {
        let mut model = model_with_api("gpt-4o", "openai", ApiKind::OpenAICompletions);
        model.context_window = 131_072;
        let state = ConfigState::new(
            AnieConfig::default(),
            RuntimeState::default(),
            model,
            ThinkingLevel::Off,
            None,
        );
        assert_eq!(state.active_ollama_num_ctx_override(), None);
    }

    #[test]
    fn env_cap_tunes_or_disables_the_default_clamp() {
        assert_eq!(default_num_ctx_clamp(131_072, Some("65536")), Some(65_536));
        // A cap at/above the window passes the full window through.
        assert_eq!(default_num_ctx_clamp(131_072, Some("200000")), None);
        // Garbage falls back to the built-in cap rather than erroring.
        assert_eq!(default_num_ctx_clamp(131_072, Some("garbage")), Some(32_768));
    }

    #[test]
    fn active_ollama_num_ctx_override_returns_some_when_runtime_entry_present() {
        let mut runtime_state = RuntimeState::default();
        runtime_state
            .ollama_num_ctx_overrides
            .insert("ollama:qwen3:32b".into(), 16_384);
        let state = ConfigState::new(
            AnieConfig::default(),
            runtime_state,
            model_with_api("qwen3:32b", "ollama", ApiKind::OllamaChatApi),
            ThinkingLevel::Off,
            None,
        );

        assert_eq!(state.active_ollama_num_ctx_override(), Some(16_384));
    }

    #[test]
    fn active_ollama_num_ctx_override_keyed_by_provider_and_model_tuple() {
        let mut runtime_state = RuntimeState::default();
        runtime_state
            .ollama_num_ctx_overrides
            .insert("ollama1:qwen3:32b".into(), 16_384);
        runtime_state
            .ollama_num_ctx_overrides
            .insert("ollama2:qwen3:32b".into(), 65_536);
        let mut state = ConfigState::new(
            AnieConfig::default(),
            runtime_state,
            model_with_api("qwen3:32b", "ollama1", ApiKind::OllamaChatApi),
            ThinkingLevel::Off,
            None,
        );

        assert_eq!(state.active_ollama_num_ctx_override(), Some(16_384));

        state.set_model(model_with_api(
            "qwen3:32b",
            "ollama2",
            ApiKind::OllamaChatApi,
        ));

        assert_eq!(state.active_ollama_num_ctx_override(), Some(65_536));
    }

    #[test]
    fn active_ollama_num_ctx_override_ignores_non_ollama_chat_api_model() {
        let mut runtime_state = RuntimeState::default();
        runtime_state
            .ollama_num_ctx_overrides
            .insert("ollama:qwen3:32b".into(), 16_384);
        let state = ConfigState::new(
            AnieConfig::default(),
            runtime_state,
            model("qwen3:32b", "ollama"),
            ThinkingLevel::Off,
            None,
        );

        assert_eq!(state.active_ollama_num_ctx_override(), None);
    }

    #[test]
    fn effective_ollama_context_window_uses_override_when_present() {
        let mut runtime_state = RuntimeState::default();
        runtime_state
            .ollama_num_ctx_overrides
            .insert("ollama:qwen3:32b".into(), 16_384);
        let state = ConfigState::new(
            AnieConfig::default(),
            runtime_state,
            model_with_api("qwen3:32b", "ollama", ApiKind::OllamaChatApi),
            ThinkingLevel::Off,
            None,
        );

        assert_eq!(state.effective_ollama_context_window(), 16_384);
    }

    #[test]
    fn effective_ollama_context_window_uses_model_context_when_no_override() {
        let state = ConfigState::new(
            AnieConfig::default(),
            RuntimeState::default(),
            model_with_api("qwen3:32b", "ollama", ApiKind::OllamaChatApi),
            ThinkingLevel::Off,
            None,
        );

        assert_eq!(state.effective_ollama_context_window(), 32_768);
    }
}
