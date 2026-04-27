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

use anie_provider::ProviderError;
use thiserror::Error;

/// Maximum number of characters of the provider's verbatim error
/// body to include in the user-facing message. Anything longer
/// gets truncated with an ellipsis. The body is reproduced for
/// debugging context — the actionable hint comes from the
/// computed `/context-length` suggestion, not from the body.
const PROVIDER_BODY_EXCERPT_CHARS: usize = 200;

/// Render a friendly, multi-line user-facing message for the
/// subset of `ProviderError` variants where anie has additional
/// context to add (e.g. a recovery suggestion that's more
/// specific than the variant's `Display`).
///
/// Returns `None` for variants where the default `Display` is
/// already adequate; callers fall back to `error.to_string()`.
///
/// Today: only `ModelLoadResources` produces a rich message —
/// it carries the suggested-`num_ctx` recovery target and we
/// can name the active model. Other variants pass through.
///
/// Format for `ModelLoadResources` (matches the spec in
/// `docs/ollama_load_failure_recovery/README.md` PR 3):
///
/// ```text
/// Model '<provider>:<id>' couldn't load at num_ctx=<requested>.
/// Halved retry at num_ctx=<tried> also failed.
///
/// Try /context-length <suggested> or smaller.
///
/// (provider response: <body excerpt>)
/// ```
///
/// `tried` is computed from the controller-supplied
/// `requested_num_ctx / 2` because the inner halved-retry in
/// `OllamaChatProvider::stream` runs before the variant ever
/// reaches `RetryPolicy::decide`. By the time we render this
/// message, the halved attempt has already been tried.
pub(crate) fn render_user_facing_provider_error(
    error: &ProviderError,
    requested_num_ctx: u64,
    model_provider: &str,
    model_id: &str,
) -> Option<String> {
    match error {
        ProviderError::ModelLoadResources {
            body,
            suggested_num_ctx,
        } => {
            let tried = (requested_num_ctx / 2).max(2_048);
            let body_excerpt = truncate_excerpt(body, PROVIDER_BODY_EXCERPT_CHARS);
            Some(format!(
                "Model '{model_provider}:{model_id}' couldn't load at num_ctx={requested_num_ctx}. \
                 Halved retry at num_ctx={tried} also failed.\n\
                 \n\
                 Try /context-length {suggested_num_ctx} or smaller.\n\
                 \n\
                 (provider response: {body_excerpt})"
            ))
        }
        ProviderError::ModelOutputMalformed(body) => {
            let body_excerpt = truncate_excerpt(body, PROVIDER_BODY_EXCERPT_CHARS);
            Some(format!(
                "Model '{model_provider}:{model_id}' emitted output the provider couldn't parse. \
                 Anie retried automatically; if you're seeing this message the retry also failed.\n\
                 \n\
                 This usually happens when context pressure causes a smaller model to produce a \
                 malformed tool call. Try /context-length with a smaller value, switch to a \
                 larger model, or shorten your prompt.\n\
                 \n\
                 (provider response: {body_excerpt})"
            ))
        }
        _ => None,
    }
}

/// Truncate `s` to at most `max_chars` characters, appending `…`
/// when truncation occurs. Preserves UTF-8 char boundaries by
/// counting `chars()`, not bytes.
fn truncate_excerpt(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

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

    #[test]
    fn user_error_for_model_load_resources_includes_context_length_hint() {
        let error = ProviderError::ModelLoadResources {
            body: r#"{"error":"model requires more system memory (56.0 GiB) than is available (50.3 GiB)"}"#
                .into(),
            suggested_num_ctx: 32_768,
        };
        let message = render_user_facing_provider_error(&error, 131_072, "ollama", "qwen3.5:35b")
            .expect("ModelLoadResources must produce a rich message");
        assert!(
            message.contains("/context-length 32768"),
            "must point at the slash command with the suggested value; got:\n{message}"
        );
        assert!(
            message.contains("ollama:qwen3.5:35b"),
            "must name the model so the user knows which model failed"
        );
    }

    #[test]
    fn user_error_message_mentions_attempted_halved_value() {
        // The inner halved-retry runs before this variant
        // reaches the user. The message must surface BOTH the
        // original requested num_ctx AND the halved retry's
        // value so the user sees the full progression.
        let error = ProviderError::ModelLoadResources {
            body: "model requires more system memory".into(),
            suggested_num_ctx: 65_536,
        };
        let message = render_user_facing_provider_error(&error, 262_144, "ollama", "qwen3.5:9b")
            .expect("rich message");
        assert!(
            message.contains("num_ctx=262144"),
            "must show the originally requested value; got:\n{message}"
        );
        assert!(
            message.contains("num_ctx=131072"),
            "must show the halved retry value (262144/2); got:\n{message}"
        );
        assert!(
            message.contains("/context-length 65536"),
            "must show the next-step suggestion; got:\n{message}"
        );
    }

    #[test]
    fn user_error_renders_provider_body_excerpt() {
        let body = "VeryLongRepeatedDiagnosticMessage".repeat(20);
        let error = ProviderError::ModelLoadResources {
            body: body.clone(),
            suggested_num_ctx: 32_768,
        };
        let message = render_user_facing_provider_error(&error, 131_072, "ollama", "qwen3:32b")
            .expect("rich message");
        assert!(
            message.contains("provider response:"),
            "must label the body excerpt"
        );
        // Body is repeated 20× (33 chars × 20 = 660 chars).
        // Truncation kicks in at 200 chars.
        assert!(
            message.contains('…'),
            "long bodies must be truncated with an ellipsis"
        );
        // Sanity: the message body (after the body-excerpt
        // marker) must be shorter than the raw body.
        assert!(
            message.len() < body.len() + 200,
            "truncation should bound the message size"
        );
    }

    #[test]
    fn user_error_short_body_is_not_truncated() {
        // Boundary: a body that fits within
        // PROVIDER_BODY_EXCERPT_CHARS must round-trip verbatim.
        let body = "short body";
        let error = ProviderError::ModelLoadResources {
            body: body.into(),
            suggested_num_ctx: 32_768,
        };
        let message = render_user_facing_provider_error(&error, 131_072, "ollama", "qwen3:32b")
            .expect("rich message");
        assert!(
            message.contains("short body"),
            "short body must round-trip verbatim"
        );
        assert!(
            !message.contains('…'),
            "short body must not get a truncation marker"
        );
    }

    #[test]
    fn user_error_returns_none_for_variants_without_rich_rendering() {
        // Other ProviderError variants fall through to the
        // default Display via the controller's
        // error.to_string() path. The renderer must not
        // misclassify them as needing the rich rendering.
        for error in [
            ProviderError::Auth("nope".into()),
            ProviderError::ContextOverflow("too many".into()),
            ProviderError::ResponseTruncated,
            ProviderError::EmptyAssistantResponse,
        ] {
            assert_eq!(
                render_user_facing_provider_error(&error, 131_072, "ollama", "qwen3:32b"),
                None,
                "non-ModelLoadResources variants must return None: {error:?}"
            );
        }
    }

    #[test]
    fn truncate_excerpt_handles_utf8_boundaries() {
        // Multi-byte UTF-8 characters must not split. Counting
        // by chars() rather than bytes prevents the truncation
        // from creating invalid UTF-8.
        let s = "日本語".repeat(100); // 300 chars, 900 bytes
        let truncated = truncate_excerpt(&s, 50);
        assert!(
            truncated.chars().count() <= 51,
            "truncation respects char count; got {} chars",
            truncated.chars().count()
        );
        assert!(
            std::str::from_utf8(truncated.as_bytes()).is_ok(),
            "truncation produces valid UTF-8"
        );
    }
}
