//! `ConfigState` — configured runtime knobs owned by the controller.
//!
//! Bundles the static config, persisted runtime defaults, and the
//! currently-selected model / thinking state so controller code can
//! delegate config-focused mutations instead of touching five bare
//! fields.

use anyhow::{Result, anyhow};

use anie_config::{AnieConfig, CliOverrides, load_config};
use anie_provider::{Model, ThinkingLevel};
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

    pub(crate) fn persist_runtime_state(&mut self, session_id: &str) {
        self.runtime_state.provider = Some(self.current_model.provider.clone());
        self.runtime_state.model = Some(self.current_model.id.clone());
        self.runtime_state.thinking = Some(self.current_thinking);
        self.runtime_state.last_session_id = Some(session_id.to_string());
        if let Err(error) = save_runtime_state(&self.runtime_state) {
            tracing::warn!(%error, "failed to persist runtime state");
        }
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
            fallback_model_from_provider(current_provider, current_model, &config, &model_catalog)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_state::load_runtime_state_from;
    use anie_provider::{ApiKind, CostPerMillion, ModelCompat};

    fn model(id: &str, provider: &str) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            provider: provider.into(),
            api: ApiKind::OpenAICompletions,
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
}
