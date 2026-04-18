//! Handles that decompose `ControllerState` into focused owners.
//!
//! - `SessionHandle` owns the session file and the paths around it.
//! - `SystemPromptCache` owns the cached system-prompt text and
//!   knows when to rebuild it from project-context files.

pub(crate) mod prompt_cache;
pub(crate) mod session_handle;

pub(crate) use prompt_cache::SystemPromptCache;
pub(crate) use session_handle::SessionHandle;
