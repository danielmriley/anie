use std::{
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
}

/// Return the default runtime-state file path.
#[must_use]
pub fn state_file_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".anie/state.json"))
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
    fs::write(path, contents)
        .with_context(|| format!("failed to write runtime state {}", path.display()))?;
    Ok(())
}
