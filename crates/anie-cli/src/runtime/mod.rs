//! Handles that decompose `ControllerState` into focused owners.
//!
//! - `ConfigState` owns config.toml + runtime-state + the current
//!   model/thinking selections.
//! - `SessionHandle` owns the session file and the paths around it.
//! - `SystemPromptCache` owns the cached system-prompt text and
//!   knows when to rebuild it from project-context files.

pub(crate) mod config_state;
pub(crate) mod prompt_cache;
pub(crate) mod session_handle;

pub(crate) use config_state::ConfigState;
pub(crate) use prompt_cache::SystemPromptCache;
pub(crate) use session_handle::SessionHandle;
