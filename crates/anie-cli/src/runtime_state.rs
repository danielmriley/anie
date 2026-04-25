use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use anie_provider::ThinkingLevel;

/// Mutable, non-secret runtime defaults stored separately from config.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeState {
    /// Last used provider name.
    pub provider: Option<String>,
    /// Last used model identifier.
    pub model: Option<String>,
    /// Last used thinking level.
    pub thinking: Option<ThinkingLevel>,
    /// Last active session ID.
    pub last_session_id: Option<String>,
    /// Per-model Ollama native `num_ctx` overrides, keyed by
    /// "{provider}:{model_id}".
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub ollama_num_ctx_overrides: HashMap<String, u64>,
}

/// Return the default runtime-state file path.
#[must_use]
pub fn state_file_path() -> Option<PathBuf> {
    anie_config::anie_state_json_path()
}

/// Load runtime state from the standard location.
pub fn load_runtime_state() -> Result<RuntimeState> {
    let Some(path) = state_file_path() else {
        return Ok(RuntimeState::default());
    };
    load_runtime_state_from(&path)
}

/// Load runtime state from an explicit path.
pub fn load_runtime_state_from(path: &Path) -> Result<RuntimeState> {
    if !path.exists() {
        return Ok(RuntimeState::default());
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read runtime state {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse runtime state {}", path.display()))
}

/// Persist runtime state to the standard location.
pub fn save_runtime_state(state: &RuntimeState) -> Result<()> {
    let Some(path) = state_file_path() else {
        return Ok(());
    };
    save_runtime_state_to(&path, state)
}

/// Persist runtime state to an explicit path.
pub fn save_runtime_state_to(path: &Path, state: &RuntimeState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let contents =
        serde_json::to_string_pretty(state).context("failed to serialize runtime state")?;
    anie_config::atomic_write(path, contents.as_bytes())
        .with_context(|| format!("failed to write runtime state {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_state_forward_compat_loads_state_without_num_ctx_overrides() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("state.json");
        fs::write(
            &path,
            r#"{
              "provider": "ollama",
              "model": "qwen3:32b",
              "thinking": "Off",
              "last_session_id": "session-1"
            }"#,
        )
        .expect("write state");

        let state = load_runtime_state_from(&path).expect("load runtime state");

        assert_eq!(state.provider.as_deref(), Some("ollama"));
        assert_eq!(state.model.as_deref(), Some("qwen3:32b"));
        assert!(state.ollama_num_ctx_overrides.is_empty());
    }

    #[test]
    fn runtime_state_serializes_num_ctx_overrides_when_non_empty() {
        let mut state = RuntimeState::default();
        state
            .ollama_num_ctx_overrides
            .insert("ollama:qwen3:32b".into(), 16_384);

        let serialized = serde_json::to_string(&state).expect("serialize state");

        assert!(serialized.contains("ollama_num_ctx_overrides"));
        assert!(serialized.contains("ollama:qwen3:32b"));
        assert!(serialized.contains("16384"));
    }

    #[test]
    fn runtime_state_omits_num_ctx_overrides_field_when_empty() {
        let serialized = serde_json::to_string(&RuntimeState::default()).expect("serialize state");

        assert!(!serialized.contains("ollama_num_ctx_overrides"));
    }
}
