//! Handles that decompose `ControllerState` into focused owners.
//!
//! `SessionHandle` owns the session file and the paths around it.
//! Future additions: `ConfigState` (plan 03 phase 5, part 2).

pub(crate) mod session_handle;

pub(crate) use session_handle::SessionHandle;
