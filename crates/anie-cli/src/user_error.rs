//! Containment layer for user-visible slash-command errors.
//!
//! The interactive controller treats two very different failure
//! modes uniformly today: a malformed user argument (like
//! `/thinking bogus`) and a real internal fault (a poisoned mutex,
//! a session write that can't flush). Both bubble up through
//! `?` and terminate the controller task, which then leaves the TUI
//! sending `UiAction`s into a dead channel.
//!
//! `UserCommandError` marks the first kind. `HandleError`
//! classifies a `try_handle_action` result so the dispatcher can
//! surface user errors as system messages while still propagating
//! unexpected failures.

use thiserror::Error;

/// An error caused by malformed user input, not by a bug or
/// external failure. These are displayed as system messages and
/// never terminate the controller loop.
#[derive(Debug, Error)]
pub(crate) enum UserCommandError {
    /// `/thinking <value>` where `<value>` is not one of the
    /// supported levels.
    #[error("invalid thinking level '{0}' (expected: off, low, medium, high)")]
    InvalidThinkingLevel(String),
    /// `/session <id>` where no session with that ID exists in the
    /// configured sessions directory.
    #[error("unknown session '{0}'")]
    UnknownSession(String),
    /// `/model <id>` where the requested model cannot be resolved
    /// against the current catalog.
    #[error("unknown model '{0}'")]
    UnknownModel(String),
}

/// Classification of a `try_handle_action` failure.
///
/// User errors (bad arg, unknown session) surface as system
/// messages and return `Ok(())` to the run loop. Fatal errors
/// propagate and terminate the controller, same as before.
pub(crate) enum HandleError {
    /// User supplied a malformed argument. Display and continue.
    User(UserCommandError),
    /// Genuine internal failure. Terminate the controller.
    Fatal(anyhow::Error),
}

impl From<UserCommandError> for HandleError {
    fn from(error: UserCommandError) -> Self {
        Self::User(error)
    }
}

impl From<anyhow::Error> for HandleError {
    /// Classify an `anyhow::Error`. If it wraps a
    /// `UserCommandError` (via `.context(user_err)` or direct
    /// conversion), re-raise it as `User`. Otherwise it's
    /// `Fatal`.
    fn from(error: anyhow::Error) -> Self {
        match error.downcast::<UserCommandError>() {
            Ok(user) => Self::User(user),
            Err(other) => Self::Fatal(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downcast_promotes_user_error_from_anyhow() {
        let anyhow_err: anyhow::Error =
            UserCommandError::InvalidThinkingLevel("bogus".into()).into();
        let handle: HandleError = anyhow_err.into();
        assert!(matches!(
            handle,
            HandleError::User(UserCommandError::InvalidThinkingLevel(ref s)) if s == "bogus"
        ));
    }

    #[test]
    fn downcast_keeps_non_user_errors_fatal() {
        let anyhow_err = anyhow::anyhow!("random internal failure");
        let handle: HandleError = anyhow_err.into();
        assert!(matches!(handle, HandleError::Fatal(_)));
    }

    #[test]
    fn display_includes_expected_values_for_thinking() {
        let err = UserCommandError::InvalidThinkingLevel("zzz".into());
        let rendered = err.to_string();
        assert!(rendered.contains("zzz"));
        assert!(rendered.contains("off"));
        assert!(rendered.contains("low"));
        assert!(rendered.contains("medium"));
        assert!(rendered.contains("high"));
    }

    #[test]
    fn display_renders_unknown_session() {
        let err = UserCommandError::UnknownSession("sess-123".into());
        assert_eq!(err.to_string(), "unknown session 'sess-123'");
    }

    #[test]
    fn display_renders_unknown_model() {
        let err = UserCommandError::UnknownModel("gpt-99".into());
        assert_eq!(err.to_string(), "unknown model 'gpt-99'");
    }
}
